"""Steady-state summary of instrumented runs: 5 s windows ending between FROM and TO seconds (v1: 15-40),
as min-max ranges, plus the echo probe pooled over those windows. argv: runs_dir name... [--from 15 --to 40]"""
import json, sys
args = sys.argv[1:]
lo, hi = 15.0, 40.0
if "--from" in args: i = args.index("--from"); lo = float(args[i + 1]); del args[i:i + 2]
if "--to" in args: i = args.index("--to"); hi = float(args[i + 1]); del args[i:i + 2]
root, names = args[0], args[1:]
def rng(values, fmt="%.1f"):
    if not values: return "-"
    a, b = min(values), max(values)
    return fmt % a if fmt % a == fmt % b else (fmt % a) + "-" + (fmt % b)
def q(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, round((len(values) - 1) * p))] if values else float("nan")
for name in names:
    rows = [json.loads(line) for line in open(f"{root}/{name}.jsonl")]
    adapter = next((r for r in rows if "adapter" in r), {})
    win = [r for r in rows if "t" in r and lo < r["t"] <= hi + 1]
    probes = [v for r in win for v in r.get("probe_all_ms", [])]
    print(f"== {name}: {adapter.get('adapter')} {adapter.get('backend')} {adapter.get('device_type')} {adapter.get('present_mode')}, "
          f"{len(win)} windows, px {win[0]['px'] if win else '-'}")
    print(f"   fps {rng([r['fps'] for r in win])}  interval p50 {rng([r['p50_ms'] for r in win])}  p95 {rng([r['p95_ms'] for r in win])}"
          f"  p99 {rng([r['p99_ms'] for r in win])} ms  >20ms {rng([r['over_20ms'] for r in win], '%d')}  >34ms {rng([r['over_34ms'] for r in win], '%d')}"
          f"  cpu p50 {rng([r['cpu_p50_ms'] for r in win])} p95 {rng([r['cpu_p95_ms'] for r in win])} ms")
    if probes:
        print(f"   echo probe n={len(probes)} p50 {q(probes, .5):.1f} p95 {q(probes, .95):.1f} max {max(probes):.1f} ms")
