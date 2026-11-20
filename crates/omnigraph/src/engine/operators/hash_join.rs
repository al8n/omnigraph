//! `HashJoinExec`: a traversal's destination table read once (the build, a
//! table `ScanExec`) and hashed on its id, probed by the traversal's rows in
//! their order. The `HashJoin` node declares its switch, and both of its
//! sides are branches of this one operator, taken on what the run observes:
//!
//! - The probe is read before the build. A join whose node declares the
//!   `id_lookup` fallback holds the probe's batches, in order and each under
//!   its own reservation, while their rows are too few for the planner's own
//!   rule to choose the hash join (`HASH_JOIN_RATIO` times the held rows
//!   below the destination table's row count at the pinned snapshot). Once
//!   they are enough, or once they reach `HELD_ROWS_CAP` rows or one part in
//!   `HELD_POOL_DIVISOR` of the pool in bytes (a hold that stays small keeps
//!   the build's pool share intact), the build is drained and the held
//!   batches go through the hash in order. A probe that ends
//!   first is looked up by id, held batch by held batch, and the destination
//!   table is never read whole (`small_probe_lookups`).
//! - A join that declares no fallback, or whose destination row count the
//!   snapshot does not hold, drains the build at its first non-empty probe
//!   batch.
//! - A memory refusal while the build is drained releases the build and
//!   reaches the destination rows through the per-slice id lookup instead,
//!   the held batches first, when the node declares that fallback
//!   (`hash_build_fallbacks`).
//!
//! An empty traversal (a source the gate proved empty) holds nothing and
//! never reads the destination; an empty build stops the probe once drained.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt32Array};
use arrow_schema::SchemaRef;
use datafusion::common::{DataFusionError, Result as DfResult};
use datafusion::execution::TaskContext;
use datafusion::physical_plan::metrics::{
    Count, ExecutionPlanMetricsSet, Gauge, MetricBuilder, MetricsSet,
};
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties, SendableRecordBatchStream,
};
use futures::future::BoxFuture;
use omnigraph_compiler::catalog::Catalog;
use omnigraph_compiler::ir::{IRExpr, ParamMap};
use omnigraph_planner::{AccessPath, HASH_JOIN_RATIO};

use super::key_groups::KeyGroups;
use super::memory::WorkMemory;
use super::scan::Lookup;
use super::shape::{BatchSender, CollectedSide, Flow, Streamed, Streaming, work};
use super::{ScanExec, Switch, external};
use crate::db::Snapshot;
use crate::engine::scan::hconcat_batches;
use crate::engine::search::NeededColumns;
use crate::error::{OmniError, Result};

/// The refusal name the pool reports when an output batch does not fit.
const OUTPUT: &str = "hash join output";

/// The refusal name the pool reports when a probe batch held while the join
/// takes no side does not fit.
const HELD: &str = "hash join held probe";

/// The probe rows held before the build is drained: a probe beyond 16 lookup
/// slices of 256 rows takes the planned build.
const HELD_ROWS_CAP: u64 = 4096;

/// The part of the pool the held probe may take; the hold keeps the build's
/// pool share intact.
const HELD_POOL_DIVISOR: u64 = 64;

/// What reaching the destination rows takes on either side of the switch:
/// the destination type, the binding whose ids the probe carries, the pushed
/// filters and the projection.
#[derive(Clone)]
pub(crate) struct LookupSpec {
    pub(crate) type_name: String,
    pub(crate) binding: String,
    pub(crate) filters: Vec<IRExpr>,
    pub(crate) projection: Option<NeededColumns>,
    pub(crate) params: Arc<ParamMap>,
    pub(crate) snapshot: Snapshot,
    pub(crate) catalog: Arc<Catalog>,
}

impl LookupSpec {
    /// `<binding>.<id>`, the column both sides carry.
    pub(crate) fn id_column(&self) -> String {
        format!("{}.{}", self.binding, self.catalog.system_columns.id)
    }
}

pub(crate) struct HashJoinExec {
    probe: Arc<dyn ExecutionPlan>,
    build: Arc<dyn ExecutionPlan>,
    fallback: Option<AccessPath>,
    /// The destination table's row count at the pinned snapshot, the
    /// manifest count the planner's access-path rule read; `None` when the
    /// snapshot holds none.
    destination_rows: Option<u64>,
    lookup: Arc<LookupSpec>,
    shape: Streaming,
    metrics: ExecutionPlanMetricsSet,
}

impl HashJoinExec {
    pub(crate) fn try_new(
        probe: Arc<dyn ExecutionPlan>,
        build: Arc<dyn ExecutionPlan>,
        fallback: Option<AccessPath>,
        destination_rows: Option<u64>,
        lookup: LookupSpec,
    ) -> Result<Self> {
        if fallback == Some(AccessPath::HashJoin) {
            return Err(OmniError::manifest_internal(
                "a hash join declares id_lookup or no fallback",
            ));
        }
        let schema = ScanExec::input_schema(
            &probe.schema(),
            &lookup.type_name,
            &lookup.binding,
            lookup.projection.as_ref(),
            &lookup.catalog,
        )?;
        let id_column = lookup.id_column();
        if build.schema().column_with_name(&id_column).is_none() {
            return Err(OmniError::manifest_internal(format!(
                "destination scan projects no '{id_column}'"
            )));
        }
        Ok(Self {
            probe,
            build,
            fallback,
            destination_rows,
            lookup: Arc::new(lookup),
            shape: Streaming::new(schema),
            metrics: ExecutionPlanMetricsSet::new(),
        })
    }

    /// The destination rows the held probe rows are weighed against: a join
    /// that may take the id lookup holds its probe until the planner's rule
    /// (`rows <= probe rows × HASH_JOIN_RATIO`) chooses the hash join for the
    /// rows held; any other join decides at its first non-empty probe batch.
    fn decision_rows(&self) -> u64 {
        match (self.fallback, self.destination_rows) {
            (Some(AccessPath::IdLookup), Some(rows)) => rows,
            _ => 0,
        }
    }
}

impl fmt::Debug for HashJoinExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashJoinExec")
            .field("on", &self.lookup.id_column())
            .field("fallback", &self.fallback)
            .field("destination_rows", &self.destination_rows)
            .finish_non_exhaustive()
    }
}

impl DisplayAs for HashJoinExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HashJoinExec: on={}", self.lookup.id_column())?;
        match self.fallback {
            Some(AccessPath::IdLookup) => write!(f, ", fallback=id_lookup"),
            Some(AccessPath::HashJoin) | None => write!(f, ", fallback=none"),
        }
    }
}

/// The drained build side: its rows and, per distinct non-null id, the first
/// row holding it, as the [`KeyGroups`] of its id column (one vectorized
/// `create_hashes` pass, no id copied out of the Arrow column), held under
/// `memory` for as long as the probe runs.
struct Built {
    batch: RecordBatch,
    ids: KeyGroups,
    _memory: WorkMemory,
}

async fn build_side(
    build: &Arc<dyn ExecutionPlan>,
    id_column: &str,
    memory: &WorkMemory,
) -> DfResult<Built> {
    let side = CollectedSide::collect(build, memory, "hash join build").await?;
    let work = side.held;
    let batch = side.batch;
    let ids = batch
        .column_by_name(id_column)
        .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| {
            external(OmniError::manifest_internal(format!(
                "destination scan must return Utf8 '{id_column}'"
            )))
        })?;
    u32::try_from(ids.len()).map_err(|_| {
        external(OmniError::manifest_internal(
            "destination scan exceeds row index range",
        ))
    })?;
    // The groups plus the hash buffer, one `u64` per row; the buffer's share
    // is released once the groups are built.
    let before = work.charged();
    let held = KeyGroups::group_bytes(ids.len());
    work.grow(held)?;
    work.entries::<u64>(ids.len())?;
    let ids = KeyGroups::build(ids)?;
    work.shrink_to(before.saturating_add(held));
    Ok(Built {
        batch,
        ids,
        _memory: work,
    })
}

/// `output`'s columns in the declared order, by name: the build scan also
/// carries the columns its pushed filters read, which the schema omits.
fn declared_columns(output: &RecordBatch, declared: &SchemaRef) -> Result<RecordBatch> {
    let columns = declared
        .fields()
        .iter()
        .map(|field| {
            output.column_by_name(field.name()).cloned().ok_or_else(|| {
                OmniError::manifest_internal(format!(
                    "hash join output has no column '{}' the dependent scan declares",
                    field.name()
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    RecordBatch::try_new(Arc::clone(declared), columns).map_err(OmniError::arrow_internal)
}

impl ExecutionPlan for HashJoinExec {
    fn name(&self) -> &str {
        "HashJoinExec"
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        self.shape.properties()
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.probe, &self.build]
    }

    fn with_new_children(
        self: Arc<Self>,
        mut children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        assert_eq!(children.len(), 2, "HashJoinExec has a probe and a build");
        let build = children.pop().expect("build child");
        let probe = children.pop().expect("probe child");
        Ok(Arc::new(
            Self::try_new(
                probe,
                build,
                self.fallback,
                self.destination_rows,
                LookupSpec::clone(&self.lookup),
            )
            .map_err(external)?,
        ))
    }

    fn execute(
        &self,
        partition: usize,
        ctx: Arc<TaskContext>,
    ) -> DfResult<SendableRecordBatchStream> {
        assert_eq!(partition, 0, "HashJoinExec has one partition");
        let memory = work(Arc::clone(&ctx), "HashJoinExec", &self.metrics)?;
        memory.metric("input_rows", 0);
        let held_bytes =
            usize::try_from(memory.pool_bytes() / HELD_POOL_DIVISOR).unwrap_or(usize::MAX);
        let probe = Arc::clone(&self.probe);
        let body = Probe {
            build: Arc::clone(&self.build),
            lookup: Arc::clone(&self.lookup),
            declared: self.schema(),
            fallback: self.fallback,
            decision_rows: self.decision_rows(),
            held_bytes,
            switch: Switch::gauge(&self.metrics),
            fallbacks: MetricBuilder::new(&self.metrics).counter("hash_build_fallbacks", 0),
            small_probe_lookups: MetricBuilder::new(&self.metrics)
                .counter("small_probe_lookups", 0),
            side: Side::Pending,
            held: Held::default(),
        };
        Ok(self
            .shape
            .produce(memory, &self.metrics, move || probe.execute(0, ctx), body))
    }
}

/// The per-batch body of the join over the probe: the probe's batches are
/// held until they decide the side, then joined through it in order.
struct Probe {
    build: Arc<dyn ExecutionPlan>,
    lookup: Arc<LookupSpec>,
    declared: SchemaRef,
    fallback: Option<AccessPath>,
    /// The destination rows the held probe rows are weighed against
    /// ([`HashJoinExec::decision_rows`]); zero decides at the first batch.
    decision_rows: u64,
    /// The bytes the held probe may take before the build is drained
    /// whatever its rows: one part in `HELD_POOL_DIVISOR` of the pool, so a
    /// large probe leaves the build its share (`HELD_ROWS_CAP` bounds the
    /// rows the same way).
    held_bytes: usize,
    switch: Gauge,
    /// Memory refusals of the build that sent the join to its fallback.
    fallbacks: Count,
    /// Probes that ended with too few rows for the build, looked up by id.
    small_probe_lookups: Count,
    side: Side,
    held: Held,
}

/// How the probe reaches the destination rows, once a side was taken.
enum Side {
    /// No side yet: the probe's batches are held.
    Pending,
    Hashed(Box<Built>),
    Lookup(Lookup),
    /// The build held no row: nothing joins, the probe stops.
    Empty,
}

/// The probe batches a pending join holds, in arrival order, each charged
/// to its own reservation, released once the batch is joined or dropped. The
/// hold stays small (`HELD_ROWS_CAP` rows, `HELD_POOL_DIVISOR` of the pool in
/// bytes) so that a large probe still finds the pool free for its build.
#[derive(Default)]
struct Held {
    batches: VecDeque<(RecordBatch, WorkMemory)>,
    rows: u64,
    bytes: usize,
}

impl Probe {
    /// Hold `batch` until the join takes a side; whether the batches held
    /// now decide it: enough rows for the planner's rule, or the row or byte
    /// cap, which keeps the hold from starving the build of its pool share.
    fn hold(&mut self, batch: RecordBatch, memory: &WorkMemory) -> DfResult<bool> {
        let held = memory.child(HELD)?;
        held.hold(&batch)?;
        held.entries::<RecordBatch>(1)?;
        self.held.rows = self.held.rows.saturating_add(batch.num_rows() as u64);
        self.held.bytes = self
            .held
            .bytes
            .saturating_add(batch.get_array_memory_size());
        self.held.batches.push_back((batch, held));
        Ok(
            self.held.rows.saturating_mul(HASH_JOIN_RATIO) >= self.decision_rows
                || self.held.rows >= HELD_ROWS_CAP
                || self.held.bytes >= self.held_bytes,
        )
    }

    /// The per-slice id lookup, recorded as the side taken.
    fn lookup_side(&self) -> Side {
        Switch::IdLookup.record(&self.switch);
        Side::Lookup(Lookup {
            spec: Arc::clone(&self.lookup),
            declared: Arc::clone(&self.declared),
        })
    }

    /// Drain the build, the held batches in hand, into the hash or, on a
    /// memory refusal the node's fallback covers, into the per-slice lookup.
    async fn drain_build(&mut self, memory: &Arc<WorkMemory>) -> DfResult<()> {
        let id_column = self.lookup.id_column();
        self.side = match build_side(&self.build, &id_column, memory).await {
            Ok(built) => {
                Switch::HashJoin.record(&self.switch);
                if built.batch.num_rows() == 0 {
                    Side::Empty
                } else {
                    Side::Hashed(Box::new(built))
                }
            }
            Err(error) if self.fallback.is_some() && OmniError::is_query_memory_failure(&error) => {
                self.fallbacks.add(1);
                self.lookup_side()
            }
            Err(error) => return Err(error),
        };
        Ok(())
    }

    /// Join every held batch in arrival order, each released once joined;
    /// an empty build stops the probe and drops the rest.
    async fn flush(&mut self, memory: &Arc<WorkMemory>, sender: &BatchSender) -> DfResult<Flow> {
        while let Some((batch, held)) = self.held.batches.pop_front() {
            let flow = self.join(batch, memory, sender).await?;
            drop(held);
            if let Flow::Stop = flow {
                self.held = Held::default();
                return Ok(Flow::Stop);
            }
        }
        self.held = Held::default();
        Ok(Flow::Continue)
    }

    /// One probe batch through the side taken.
    async fn join(
        &self,
        batch: RecordBatch,
        memory: &Arc<WorkMemory>,
        sender: &BatchSender,
    ) -> DfResult<Flow> {
        match &self.side {
            Side::Hashed(built) => {
                self.hashed(built, batch, memory, sender).await?;
                Ok(Flow::Continue)
            }
            Side::Lookup(lookup) => {
                lookup.resolve(batch, memory, sender).await?;
                Ok(Flow::Continue)
            }
            Side::Empty => Ok(Flow::Stop),
            Side::Pending => Err(DataFusionError::Internal(
                "hash join probed before it took a side".into(),
            )),
        }
    }

    async fn hashed(
        &self,
        built: &Built,
        batch: RecordBatch,
        memory: &Arc<WorkMemory>,
        sender: &BatchSender,
    ) -> DfResult<()> {
        let id_column = self.lookup.id_column();
        let id_index = batch
            .schema()
            .index_of(&id_column)
            .map_err(|error| external(OmniError::manifest_internal(error.to_string())))?;
        let ids = batch
            .column(id_index)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                external(OmniError::manifest_internal(
                    "traversal output IDs must be Utf8",
                ))
            })?;
        let work = Arc::new(memory.child(OUTPUT)?);
        work.entries::<u32>(ids.len().saturating_mul(2))?;
        work.entries::<u64>(ids.len())?;
        u32::try_from(ids.len()).map_err(|_| {
            external(OmniError::manifest_internal(
                "traversal batch exceeds row index range",
            ))
        })?;
        let mut hashes = Vec::new();
        built.ids.hash(ids, &mut hashes)?;
        let mut probe_rows = Vec::with_capacity(ids.len());
        let mut build_rows = Vec::with_capacity(ids.len());
        for (row, &hash) in hashes.iter().enumerate() {
            if ids.is_null(row) {
                continue;
            }
            if let Some(group) = built.ids.group_of(hash, ids.value(row)) {
                probe_rows.push(row as u32);
                build_rows.push(built.ids.first_row(group));
            }
        }
        if probe_rows.is_empty() {
            return Ok(());
        }
        let probe_columns: Vec<usize> = (0..batch.num_columns())
            .filter(|index| *index != id_index)
            .collect();
        let probe_side = batch.project(&probe_columns)?;
        let probe_side = work.take_once(&probe_side, &UInt32Array::from(probe_rows), OUTPUT)?;
        let build_side = work.take_once(&built.batch, &UInt32Array::from(build_rows), OUTPUT)?;
        let output = hconcat_batches(&probe_side, &build_side).map_err(external)?;
        let output = declared_columns(&output, &self.declared).map_err(external)?;
        work.hold(&output)?;
        sender.send_bounded(output, work).await
    }
}

impl Streamed for Probe {
    fn batch<'a>(
        &'a mut self,
        batch: RecordBatch,
        memory: &'a Arc<WorkMemory>,
        sender: &'a BatchSender,
    ) -> BoxFuture<'a, DfResult<Flow>> {
        Box::pin(async move {
            memory.metric("input_rows", batch.num_rows());
            if batch.num_rows() == 0 {
                return Ok(Flow::Continue);
            }
            if !matches!(self.side, Side::Pending) {
                return self.join(batch, memory, sender).await;
            }
            if !self.hold(batch, memory)? {
                return Ok(Flow::Continue);
            }
            self.drain_build(memory).await?;
            self.flush(memory, sender).await
        })
    }

    fn finish<'a>(
        &'a mut self,
        memory: &'a Arc<WorkMemory>,
        sender: &'a BatchSender,
    ) -> BoxFuture<'a, DfResult<()>> {
        Box::pin(async move {
            if !matches!(self.side, Side::Pending) || self.held.batches.is_empty() {
                return Ok(());
            }
            // The probe ended with too few rows for the build: its held
            // rows are looked up by id, the destination never read whole.
            self.small_probe_lookups.add(1);
            self.side = self.lookup_side();
            self.flush(memory, sender).await.map(|_| ())
        })
    }
}
