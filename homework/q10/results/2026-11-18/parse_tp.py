import sys, collections, xml.etree.ElementTree as ET
path, mode = sys.argv[1], sys.argv[2]
root = ET.parse(path).getroot()
reg = {}
def val(el):
    if el is None: return None
    r = el.get('ref')
    if r is not None: return reg[r]
    if el.tag == 'backtrace':
        frames = []
        for f in el.findall('frame'):
            fr = f.get('ref')
            if fr is not None: frames.append(reg[fr])
            else:
                nm = f.get('name') or f.get('addr'); reg[f.get('id')] = nm; frames.append(nm)
                b = f.find('binary')
                if b is not None and b.get('id'): reg[b.get('id')] = b.get('name')
        v = frames
    elif el.tag == 'tagged-backtrace':
        v = val(el.find('backtrace'))
    elif el.tag in ('thread', 'process', 'thread-state', 'core'):
        for c in el: val(c)
        v = el.get('fmt')
    else:
        v = el.text if el.text is not None else el.get('fmt')
        for c in el: val(c)
    if el.get('id') is not None: reg[el.get('id')] = v
    return v
rows = []
for row in root.iter('row'):
    rec = {}
    for c in row:
        rec[c.tag] = val(c)
    bt = rec.get('tagged-backtrace') or rec.get('backtrace') or []
    rows.append((int(rec.get('sample-time') or 0), rec.get('thread') or '', int(rec.get('weight') or 0), bt))
rows.sort(key=lambda r: r[0])
def has(bt, key): return any(key in f for f in bt)
t0 = rows[0][0]
if mode == 'warm':
    tl = {}
    for r in rows: tl[int((r[0]-t0)/1e9)] = tl.get(int((r[0]-t0)/1e9), 0) + r[2]
    print("cpu ms per second:", {k: round(v/1e6) for k, v in sorted(tl.items())})
    lo, hi = t0 + 2_500_000_000, rows[-1][0] - 500_000_000
    print(f"span {(rows[-1][0]-t0)/1e9:.2f}s; counting samples in [2.5s, {(hi-t0)/1e9:.2f}s]")
    rows = [r for r in rows if lo < r[0] < hi]
total = sum(r[2] for r in rows)
print(f"samples={len(rows)} cpu_time={total/1e6:.0f} ms (all threads)")
threads = collections.Counter()
for r in rows: threads[r[1].split(' 0x')[0][:60]] += r[2]
print("by thread:", [(k, f"{v/total:.0%}") for k, v in threads.most_common(6)])
keys = ["graph_index", "omnigraph::engine::operators::expand", "omnigraph::engine::operators::hash_join", "omnigraph::engine::operators::scan", "omnigraph::engine::plan_source", "omnigraph_planner", "omnigraph_compiler", "capture_read_view", "lance::dataset::scanner", "lance_encoding", "lance_index", "lance_io", "lance_file", "datafusion", "arrow_", "omnigraph::table_store", "tokio::runtime", "__psynch_cvwait", "malloc", "free", "memmove", "memcpy"]
print("inclusive share:")
for k in keys:
    w = sum(r[2] for r in rows if has(r[3], k))
    if w: print(f"  {w/total:6.1%}  {k}")
leaf = collections.Counter()
for r in rows:
    if r[3]: leaf[r[3][0][:110]] += r[2]
print("top self (leaf) frames:")
for k, v in leaf.most_common(18): print(f"  {v/total:6.1%}  {k}")
incl = collections.Counter()
for r in rows:
    seen = set()
    for f in r[3]:
        if ('omnigraph' in f or 'lance' in f) and f not in seen:
            seen.add(f); incl[f[:150]] += r[2]
print("top inclusive omnigraph/lance frames:")
for k, v in incl.most_common(45): print(f"  {v/total:6.1%}  {k}")
