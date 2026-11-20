//! Throwaway in-process timing harness for one named read query.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omnigraph::db::{Omnigraph, ReadTarget};
use omnigraph_compiler::ir::ParamMap;
use omnigraph_compiler::result::QueryResult;
use tokio::runtime::Runtime;

type Res<T> = Result<T, Box<dyn Error>>;

struct Args {
    uri: String,
    query_file: String,
    query_name: String,
    iters: usize,
    warmup: usize,
    duration_secs: Option<f64>,
    expect_ids: Option<Vec<String>>,
    verbose: bool,
    profile: bool,
    prewarm: Option<String>,
}

fn parse_args() -> Res<Args> {
    let mut pos = Vec::new();
    let mut iters = 50usize;
    let mut warmup = 5usize;
    let mut duration_secs = None;
    let mut expect_ids = None;
    let mut verbose = false;
    let mut profile = false;
    let mut prewarm = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> Res<String> {
            it.next().ok_or_else(|| format!("{name} needs a value").into())
        };
        match a.as_str() {
            "--iters" => iters = val("--iters")?.parse()?,
            "--warmup" => warmup = val("--warmup")?.parse()?,
            "--duration-secs" => duration_secs = Some(val("--duration-secs")?.parse()?),
            "--expect-ids" => {
                let mut ids: Vec<String> = val("--expect-ids")?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                ids.sort();
                expect_ids = Some(ids);
            }
            "--verbose" => verbose = true,
            "--profile" => profile = true,
            "--prewarm" => prewarm = Some(val("--prewarm")?),
            s if s.starts_with("--") => return Err(format!("unknown flag {s}").into()),
            _ => pos.push(a),
        }
    }
    if pos.len() != 3 {
        return Err("usage: q10-harness <graph-uri> <query-file.gq> <query-name> [--iters N] [--warmup W] [--duration-secs S] [--expect-ids a,b,c] [--verbose] [--profile]".into());
    }
    let query_name = pos.pop().unwrap();
    let query_file = pos.pop().unwrap();
    let uri = pos.pop().unwrap();
    Ok(Args { uri, query_file, query_name, iters, warmup, duration_secs, expect_ids, verbose, profile, prewarm })
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Sorted string values of the single result column.
fn extract_values(result: &QueryResult) -> Res<Vec<String>> {
    let json = result.to_rust_json()?;
    let rows: Vec<serde_json::Value> = match json {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for row in rows {
        let v = match row {
            serde_json::Value::Object(map) => {
                if map.len() != 1 {
                    return Err(format!("expected a single result column, got {} fields", map.len()).into());
                }
                map.into_iter().next().unwrap().1
            }
            other => other,
        };
        out.push(match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        });
    }
    out.sort();
    Ok(out)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    // Nearest-rank.
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil().max(1.0) as usize;
    sorted[rank.min(n) - 1]
}

struct Stats {
    n: usize,
    min: f64,
    p50: f64,
    p90: f64,
    p95: f64,
    max: f64,
    mean: f64,
    stddev: f64,
}

fn stats(samples: &[f64]) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    let mean = s.iter().sum::<f64>() / n as f64;
    let var = s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
    Some(Stats {
        n,
        min: s[0],
        p50: percentile(&s, 50.0),
        p90: percentile(&s, 90.0),
        p95: percentile(&s, 95.0),
        max: s[n - 1],
        mean,
        stddev: var.sqrt(),
    })
}

fn stats_json(st: &Option<Stats>) -> serde_json::Value {
    match st {
        None => serde_json::Value::Null,
        Some(s) => serde_json::json!({
            "n": s.n, "min_ms": s.min, "p50_ms": s.p50, "p90_ms": s.p90,
            "p95_ms": s.p95, "max_ms": s.max, "mean_ms": s.mean, "stddev_ms": s.stddev,
        }),
    }
}

async fn run(args: Args) -> Res<()> {
    println!("pid={}", std::process::id());

    let query_source = std::fs::read_to_string(&args.query_file)
        .map_err(|e| format!("cannot read {}: {e}", args.query_file))?;
    let params = ParamMap::new();
    let target = ReadTarget::branch("main");

    // Open.
    let t = Instant::now();
    let (defaults, sources) = omnigraph::settings::from_env()?;
    let db = Omnigraph::open_read_only(&args.uri).await?;
    let session = Arc::new(db).session(defaults, sources);
    let open_ms = ms(t.elapsed());
    println!("open_ms={open_ms:.3}");

    if let Some(name) = &args.prewarm {
        let t = Instant::now();
        let r = session.query(target.clone(), &query_source, name, &params).await?;
        drop(r);
        println!("prewarm_ms={:.3}", ms(t.elapsed()));
    }

    // First query.
    let t = Instant::now();
    let first = session.query(target.clone(), &query_source, &args.query_name, &params).await?;
    let first_ms = ms(t.elapsed());
    println!("first_ms={first_ms:.3}");
    let first_values = extract_values(&first)?;
    drop(first);

    // Warm-up.
    let mut warmup_ms = Vec::new();
    for _ in 0..args.warmup {
        let t = Instant::now();
        let r = session.query(target.clone(), &query_source, &args.query_name, &params).await;
        let e = t.elapsed();
        r?;
        warmup_ms.push(ms(e));
    }
    println!(
        "warmup_ms=[{}]",
        warmup_ms.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(", ")
    );

    // Measured.
    let mut samples = Vec::new();
    let mut last: Option<QueryResult> = None;
    let mut duration_report = None;
    if let Some(secs) = args.duration_secs {
        let deadline = Duration::from_secs_f64(secs);
        let start = Instant::now();
        let mut count = 0usize;
        while start.elapsed() < deadline {
            let t = Instant::now();
            let r = session.query(target.clone(), &query_source, &args.query_name, &params).await;
            let e = t.elapsed();
            let r = r?;
            samples.push(ms(e));
            count += 1;
            last = Some(r);
        }
        let mean = samples.iter().sum::<f64>() / count.max(1) as f64;
        println!("duration iterations={count} mean_ms_per_iter={mean:.3}");
        duration_report = Some((count, mean));
    } else {
        for _ in 0..args.iters {
            let t = Instant::now();
            let r = session.query(target.clone(), &query_source, &args.query_name, &params).await;
            let e = t.elapsed();
            let r = r?;
            samples.push(ms(e));
            last = Some(r);
        }
    }

    let st = stats(&samples);
    if let Some(s) = &st {
        println!(
            "n={} min={:.3} p50={:.3} p90={:.3} p95={:.3} max={:.3} mean={:.3} stddev={:.3} (ms)",
            s.n, s.min, s.p50, s.p90, s.p95, s.max, s.mean, s.stddev
        );
    }
    if args.verbose {
        for (i, x) in samples.iter().enumerate() {
            println!("sample[{i}]={x:.3}");
        }
    }

    // Verification (outside timers).
    println!("first rows={}", first_values.len());
    if first_values.len() <= 20 {
        println!("first values={first_values:?}");
    }
    if let Some(last) = &last {
        let last_values = extract_values(last)?;
        println!("last rows={}", last_values.len());
        if last_values.len() <= 20 {
            println!("last values={last_values:?}");
        }
        if last_values != first_values {
            eprintln!("MISMATCH: first run and last run differ");
            std::process::exit(2);
        }
    }
    if let Some(expect) = &args.expect_ids {
        if *expect != first_values {
            eprintln!("MISMATCH: expected {expect:?}, got {first_values:?}");
            std::process::exit(3);
        }
        println!("expect-ids: OK");
    }

    // Per-node actuals, outside every timer.
    if args.profile {
        let executed = session
            .query_inspected(target.clone(), &query_source, &args.query_name, &params)
            .await?;
        let profile = executed.profile()?;
        match profile.to_rust_json()? {
            serde_json::Value::Array(rows) => {
                for row in rows {
                    println!("PROFILE {row}");
                }
            }
            other => println!("PROFILE {other}"),
        }
        if extract_values(&executed.result)? != first_values {
            eprintln!("MISMATCH: inspected run differs from first run");
            std::process::exit(2);
        }
    }

    let json = serde_json::json!({
        "open_ms": open_ms,
        "first_ms": first_ms,
        "warmup_ms": warmup_ms,
        "stats": stats_json(&st),
        "duration_iterations": duration_report.map(|d| d.0),
        "duration_mean_ms": duration_report.map(|d| d.1),
        "rows": first_values.len(),
    });
    println!("JSON {json}");
    Ok(())
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(64);
        }
    };
    // Same runtime construction as the CLI entry point.
    let runtime = match Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(run(args)) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
