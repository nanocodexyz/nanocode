#!/usr/bin/env python3
import argparse
import json
import math
import random
import sys


def quantile(sorted_xs, q):
    n = len(sorted_xs)
    if n == 1:
        return sorted_xs[0]
    h = (n - 1) * q
    lo = math.floor(h)
    hi = min(lo + 1, n - 1)
    return sorted_xs[lo] + (h - lo) * (sorted_xs[hi] - sorted_xs[lo])


def median(xs):
    return quantile(sorted(xs), 0.5)


def p95(xs):
    return quantile(sorted(xs), 0.95)


def mean(xs):
    return sum(xs) / len(xs)


def geomean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def interval(draws, alpha):
    s = sorted(draws)
    return [quantile(s, alpha / 2), quantile(s, 1 - alpha / 2)]


def boot_one(xs, stat, rng, reps, alpha):
    n = len(xs)
    return interval([stat([xs[rng.randrange(n)] for _ in range(n)]) for _ in range(reps)], alpha)


def boot_pairs(pairs, stats, rng, reps, alpha):
    n = len(pairs)
    draws = [[] for _ in stats]
    for _ in range(reps):
        sample = [pairs[rng.randrange(n)] for _ in range(n)]
        a = [p[0] for p in sample]
        b = [p[1] for p in sample]
        for i, stat in enumerate(stats):
            draws[i].append(stat(a, b))
    return [interval(d, alpha) for d in draws]


def summary(xs, rng, reps, alpha):
    return {
        "n": len(xs),
        "median": median(xs),
        "median_ci": boot_one(xs, median, rng, reps, alpha),
        "p95": p95(xs),
        "p95_ci": boot_one(xs, p95, rng, reps, alpha),
        "mean": mean(xs),
        "mean_ci": boot_one(xs, mean, rng, reps, alpha),
    }


def load(paths):
    rows = []
    for path in paths:
        handle = sys.stdin if path == "-" else open(path)
        for number, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            row["_order"] = len(rows)
            row["_where"] = f"{path}:{number}"
            rows.append(row)
    return rows


def cell_key(row):
    return (str(row.get("metric", "metric_ms")), str(row.get("K", "")))


def check_cell(rows, args):
    problems = []
    builds = {"A": [], "B": []}
    for row in rows:
        build = str(row.get("build"))
        if build not in builds:
            problems.append(f"{row['_where']}: build {build!r} is neither A nor B")
            continue
        value = row.get("metric_ms")
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
            problems.append(f"{row['_where']}: metric_ms missing or not finite")
            continue
        builds[build].append(row)
    for build, items in builds.items():
        commits = {r.get("commit") for r in items if r.get("commit") is not None}
        if len(commits) > 1:
            problems.append(f"build {build} mixes commits {sorted(commits)}")
    hosts = {r.get("host") for r in rows}
    if len(hosts) > 1:
        problems.append(f"cell spans hosts {sorted(map(str, hosts))}")
    by_rep = {}
    for build, items in builds.items():
        for row in items:
            slot = by_rep.setdefault(row.get("rep"), {})
            if build in slot:
                problems.append(f"rep {row.get('rep')} has two {build} rows")
            slot[build] = row
    pairs = []
    unmatched = []
    for rep, slot in sorted(by_rep.items(), key=lambda kv: (not isinstance(kv[0], int), kv[0] if isinstance(kv[0], int) else str(kv[0]))):
        if "A" not in slot or "B" not in slot:
            unmatched.append(rep)
            continue
        a, b = slot["A"], slot["B"]
        if a.get("host") != b.get("host"):
            problems.append(f"rep {rep} pairs host {a.get('host')} with {b.get('host')}")
        pairs.append((a, b))
    if unmatched:
        problems.append(f"reps without both builds: {unmatched[:10]}{'...' if len(unmatched) > 10 else ''}")
    order = sorted((r for b in builds.values() for r in b), key=lambda r: r["_order"])
    position = {id(r): i for i, r in enumerate(order)}
    apart = [a.get("rep") for a, b in pairs if abs(position[id(a)] - position[id(b)]) != 1]
    if apart and not args.allow_blocked:
        problems.append(f"reps whose A and B runs are not adjacent (not interleaved): {apart[:10]}{'...' if len(apart) > 10 else ''}")
    firsts = {"A" if position[id(a)] < position[id(b)] else "B" for a, b in pairs}
    return builds, pairs, problems, firsts


def verdict_for(pairs_ms, costs, args, rng, out):
    lower_better = not args.higher_better
    n = len(pairs_ms)
    ratios = [b / a for a, b in pairs_ms]
    leads = [(a - b) if lower_better else (b - a) for a, b in pairs_ms]
    stats = [
        lambda a, b: geomean([y / x for x, y in zip(a, b)]),
        lambda a, b: median(b) / median(a),
        lambda a, b: p95(b) / p95(a),
        lambda a, b: median([(x - y) if lower_better else (y - x) for x, y in zip(a, b)]),
    ]
    cis = boot_pairs(pairs_ms, stats, rng, args.boot, args.alpha)
    out["paired"] = {
        "pairs": n,
        "ratio_b_over_a_geomean": geomean(ratios),
        "ratio_b_over_a_geomean_ci": cis[0],
        "ratio_of_medians": median([b for _, b in pairs_ms]) / median([a for a, _ in pairs_ms]),
        "ratio_of_medians_ci": cis[1],
        "ratio_of_p95": p95([b for _, b in pairs_ms]) / p95([a for a, _ in pairs_ms]),
        "ratio_of_p95_ci": cis[2],
        "lead_ms_median": median(leads),
        "lead_ms_median_ci": cis[3],
        "lead_direction": "A - B (B faster is positive)" if lower_better else "B - A (B higher is positive)",
    }
    cheaper = None
    if costs:
        cost_ci = boot_pairs(costs, [lambda a, b: geomean([y / x for x, y in zip(a, b)])], rng, args.boot, args.alpha)[0]
        cheaper = cost_ci[1] < 1.0
        out["cost"] = {"pairs": len(costs), "ratio_b_over_a_geomean": geomean([b / a for a, b in costs]), "ratio_b_over_a_geomean_ci": cost_ci}
    target_win = False
    if args.target is not None:
        b_values = [b for _, b in pairs_ms]
        stat = p95 if args.target_stat == "p95" else median
        point = stat(b_values)
        ci = boot_one(b_values, stat, rng, args.boot, args.alpha)
        if lower_better:
            state = "MET" if ci[1] <= args.target else "MISSED" if ci[0] > args.target else "UNSURE"
        else:
            state = "MET" if ci[0] >= args.target else "MISSED" if ci[1] < args.target else "UNSURE"
        target_win = state == "MET"
        out["target"] = {
            "stat": args.target_stat,
            "target": args.target,
            "b_value": point,
            "b_value_ci": ci,
            "b_over_target": point / args.target,
            "b_over_target_ci": [ci[0] / args.target, ci[1] / args.target],
            "state": state,
        }
    lo, hi = cis[3]
    if lo > 0:
        return "WIN", "paired lead CI excludes 0 upward"
    if hi < 0:
        return "LOSS", "paired lead CI excludes 0 downward"
    if target_win:
        return "WIN", f"lead CI includes 0 but the {args.target_stat} CI is entirely within the target"
    if cheaper:
        return "TIE", "lead CI includes 0 and the cost ratio CI is below 1.0x (TIE, cheaper)"
    if cheaper is False:
        return "TIE", "lead CI includes 0 and the cost ratio CI is not below 1.0x (TIE, not cheaper)"
    return "TIE", "lead CI includes 0 and no cost field was given (TIE, cheapness unknown)"


def analyze(rows, args):
    cells = {}
    for row in rows:
        cells.setdefault(cell_key(row), []).append(row)
    results = []
    for (metric, k), items in sorted(cells.items()):
        rng = random.Random(args.seed)
        builds, pairs, problems, firsts = check_cell(items, args)
        out = {"metric": metric, "K": k, "hosts": sorted({str(r.get("host")) for r in items})}
        loads = [float(r["loadavg"]) for r in items if isinstance(r.get("loadavg"), (int, float))]
        if loads:
            out["loadavg"] = {"min": min(loads), "median": median(loads), "max": max(loads)}
        out["commits"] = {b: sorted({str(r.get("commit")) for r in builds[b]}) for b in builds}
        out["first_in_pair"] = sorted(firsts)
        for build in ("A", "B"):
            xs = [r["metric_ms"] for r in builds[build]]
            if xs:
                out[build] = summary(xs, rng, args.boot, args.alpha)
        if len(pairs) < args.min_n:
            problems.append(f"{len(pairs)} matched pairs, below the minimum {args.min_n}")
        pairs_ms = [(a["metric_ms"], b["metric_ms"]) for a, b in pairs]
        positive = all(x > 0 for p in pairs_ms for x in p)
        if not positive:
            problems.append("non-positive metric_ms, ratios undefined")
        costs = [(a["cost"], b["cost"]) for a, b in pairs if isinstance(a.get("cost"), (int, float)) and isinstance(b.get("cost"), (int, float)) and a["cost"] > 0 and b["cost"] > 0]
        if costs and len(costs) != len(pairs):
            problems.append(f"cost present on {len(costs)} of {len(pairs)} pairs")
        if len(pairs) >= 3 and positive:
            verdict, why = verdict_for(pairs_ms, costs, args, rng, out)
        else:
            verdict, why = "MISSING", "fewer than 3 usable pairs"
        if problems:
            if verdict != "MISSING":
                out["computed_verdict"] = verdict
            verdict, why = "MISSING", "; ".join(problems)
        out["verdict"] = verdict
        out["why"] = why
        results.append(out)
    return results


def fmt_ci(ci, digits):
    return f"[{ci[0]:.{digits}f}, {ci[1]:.{digits}f}]"


def render(results, args):
    pct = int(round((1 - args.alpha) * 100))
    lines = []
    for r in results:
        lines.append(f"cell metric={r['metric']} K={r['K']} hosts={','.join(r['hosts'])} commits A={','.join(r['commits']['A'])} B={','.join(r['commits']['B'])} first={','.join(r['first_in_pair'])}")
        if "loadavg" in r:
            l = r["loadavg"]
            lines.append(f"  {args.label}, load {l['min']:.2f}..{l['max']:.2f} (median {l['median']:.2f})")
        for build in ("A", "B"):
            if build in r:
                s = r[build]
                lines.append(
                    f"  {build}: n={s['n']} median {s['median']:.3f} {fmt_ci(s['median_ci'], 3)} p95 {s['p95']:.3f} {fmt_ci(s['p95_ci'], 3)} mean {s['mean']:.3f} {fmt_ci(s['mean_ci'], 3)} ms ({pct}% bootstrap)"
                )
        if "paired" in r:
            p = r["paired"]
            lines.append(
                f"  B/A paired n={p['pairs']}: geomean {p['ratio_b_over_a_geomean']:.3f}x {fmt_ci(p['ratio_b_over_a_geomean_ci'], 3)}, medians {p['ratio_of_medians']:.3f}x {fmt_ci(p['ratio_of_medians_ci'], 3)}, p95 {p['ratio_of_p95']:.3f}x {fmt_ci(p['ratio_of_p95_ci'], 3)}"
            )
            lines.append(f"  lead {p['lead_ms_median']:.3f} ms {fmt_ci(p['lead_ms_median_ci'], 3)}, {p['lead_direction']}")
        if "cost" in r:
            c = r["cost"]
            lines.append(f"  cost B/A {c['ratio_b_over_a_geomean']:.3f}x {fmt_ci(c['ratio_b_over_a_geomean_ci'], 3)}")
        if "target" in r:
            t = r["target"]
            lines.append(
                f"  target: B {t['stat']} {t['b_value']:.3f} ms {fmt_ci(t['b_value_ci'], 3)} against {t['target']} ms, {t['b_over_target']:.3f}x of target {fmt_ci(t['b_over_target_ci'], 3)}, {t['state']}"
            )
        extra = f" (would be {r['computed_verdict']})" if "computed_verdict" in r else ""
        lines.append(f"  verdict {r['verdict']}{extra}: {r['why']}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Statistics and launch verdict for ABAB-interleaved A/B timings in JSONL.")
    parser.add_argument("files", nargs="+", help="JSONL files, - for stdin; each row has build (A or B), rep, metric_ms, K, host, loadavg, and optionally metric, commit and cost")
    parser.add_argument("--target", type=float, help="fixed target for B in ms, compared with the CI of --target-stat")
    parser.add_argument("--target-stat", choices=["p95", "median"], default="p95")
    parser.add_argument("--min-n", type=int, default=30, help="matched pairs a cell needs (systems cells: 30)")
    parser.add_argument("--boot", type=int, default=10000)
    parser.add_argument("--alpha", type=float, default=0.05)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--higher-better", action="store_true")
    parser.add_argument("--allow-blocked", action="store_true", help="accept pairs whose A and B runs are not adjacent")
    parser.add_argument("--label", default="hosted Hand, unpinned")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = analyze(load(args.files), args)
    if args.json:
        print(json.dumps({"label": args.label, "alpha": args.alpha, "boot": args.boot, "seed": args.seed, "cells": results}, indent=2))
    else:
        print(render(results, args))
    sys.exit(0 if results and all(r["verdict"] != "MISSING" for r in results) else 2)


if __name__ == "__main__":
    main()
