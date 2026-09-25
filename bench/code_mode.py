#!/usr/bin/env python3
"""Apply the pre-registered code-mode decision rule (T4b, bench/plans/code-mode.md).

Reads live result files from `bench/run.py live` and `bench/acp_trials.py`, groups trials by arm
and by task group (the scripting tasks named in the manifest's `[code_mode]`, and the existing
tasks), prints one table per group and the rule's verdict. The rule and its margins come from the
manifest, so they cannot move after the results are in.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import tomllib
from pathlib import Path

from run import MANIFEST


def metrics(rows: list[dict]) -> dict:
    """Pass rate, ITE and cost per passed task (failed runs count in the numerator), means, p50 wall."""
    passes = sum(row["passed"] for row in rows)
    ites = [row.get("ite") for row in rows]
    costs = [(row.get("usage") or {}).get("cost_usd") for row in rows]
    requests = [row.get("requests") for row in rows]
    n = len(rows)
    z = 1.96
    fraction = passes / n if n else 0.0
    center = (fraction + z * z / (2 * n)) / (1 + z * z / n) if n else 0.0
    radius = z * math.sqrt(fraction * (1 - fraction) / n + z * z / (4 * n * n)) / (1 + z * z / n) if n else 0.0
    return {
        "runs": n, "passed": passes, "pass_rate": round(fraction, 3),
        "pass_rate_ci95": [round(center - radius, 3), round(center + radius, 3)],
        "mean_requests": round(statistics.mean(requests), 2) if n and None not in requests else None,
        "mean_direct_tool_calls": round(statistics.mean(row.get("direct_tool_calls", 0) for row in rows), 2) if n else None,
        "mean_nested_tool_calls": round(statistics.mean(row.get("nested_tool_calls", 0) for row in rows), 2) if n else None,
        "ite_per_passed": round(sum(ites) / passes, 1) if passes and None not in ites else None,
        "mean_ite": round(statistics.mean(ites), 1) if n and None not in ites else None,
        "usd_per_passed": round(sum(costs) / passes, 5) if passes and None not in costs else None,
        "total_usd": round(sum(cost for cost in costs if cost is not None), 5),
        "p50_wall_s": round(statistics.median(row["wall_ms"] for row in rows) / 1000, 1) if n else None,
    }


def gain(candidate: float | None, reference: float | None) -> float | None:
    """Fractional improvement of `candidate` over `reference` (positive is better, i.e. lower)."""
    if candidate is None or reference is None or reference == 0:
        return None
    return 1 - candidate / reference


def decide(runs: list[dict], rule: dict, delta_pp: float) -> tuple[str, list[dict]]:
    """The pre-registered verdict among `rule["arms"]` and why each candidate did or did not qualify."""
    scripting = set(rule["scripting_tasks"])
    reference = rule["reference"]
    def group(arm: str, which: str) -> dict:
        rows = [row for row in runs if row["harness"] == arm and
                (which == "all" or (row["case"] in scripting) == (which == "scripting"))]
        return metrics(rows)
    base = {which: group(reference, which) for which in ("all", "scripting", "existing")}
    verdicts = []
    for arm in rule["arms"]:
        if arm == reference:
            continue
        own = {which: group(arm, which) for which in ("all", "scripting", "existing")}
        noninferior = own["all"]["pass_rate"] >= base["all"]["pass_rate"] - delta_pp / 100
        requests = gain(own["scripting"]["mean_requests"], base["scripting"]["mean_requests"])
        tokens = gain(own["scripting"]["ite_per_passed"], base["scripting"]["ite_per_passed"])
        known = [value for value in (requests, tokens) if value is not None]
        scripting_win = (len(known) == 2 and max(known) >= rule["min_scripting_gain_pct"] / 100
                         and min(known) >= -rule["max_scripting_loss_pct"] / 100)
        existing = gain(own["existing"]["ite_per_passed"], base["existing"]["ite_per_passed"])
        no_regression = existing is not None and existing >= -rule["max_existing_ite_regression_pct"] / 100
        verdicts.append({"arm": arm, "pass_noninferior": noninferior, "scripting_request_gain": requests,
                         "scripting_ite_gain": tokens, "scripting_win": scripting_win, "existing_ite_gain": existing,
                         "existing_no_regression": no_regression,
                         "qualifies": noninferior and scripting_win and no_regression,
                         "all_ite_per_passed": own["all"]["ite_per_passed"]})
    qualified = [verdict for verdict in verdicts if verdict["qualifies"]]
    if not qualified:
        return reference, verdicts
    return min(qualified, key=lambda verdict: verdict["all_ite_per_passed"])["arm"], verdicts


def table(runs: list[dict], arms: list[str], cases: set[str] | None) -> str:
    header = ("| arm | runs | pass | requests | direct calls | nested calls | ITE/passed | $/passed | p50 wall s |\n"
              "|---|---|---|---|---|---|---|---|---|")
    lines = [header]
    for arm in arms:
        rows = [row for row in runs if row["harness"] == arm and (cases is None or row["case"] in cases)]
        if not rows:
            continue
        m = metrics(rows)
        def show(value):
            return "—" if value is None else str(value)
        lines.append(f"| `{arm}` | {m['runs']} | {m['passed']}/{m['runs']} | {show(m['mean_requests'])} | "
                     f"{show(m['mean_direct_tool_calls'])} | {show(m['mean_nested_tool_calls'])} | {show(m['ite_per_passed'])} | "
                     f"{show(m['usd_per_passed'])} | {show(m['p50_wall_s'])} |")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("results", type=Path, nargs="+", help="live result files (run.py live, acp_trials.py)")
    args = parser.parse_args()
    with MANIFEST.open("rb") as stream:
        manifest = tomllib.load(stream)
    rule = manifest["code_mode"]
    runs = [row for path in args.results for row in json.loads(path.read_text())["runs"]]
    scripting = set(rule["scripting_tasks"])
    every = sorted({row["case"] for row in runs})
    arms = [*rule["arms"], *rule["secondary_arms"]]
    print("## Scripting tasks\n\n" + table(runs, arms, scripting))
    print("\n## Existing tasks\n\n" + table(runs, arms, set(every) - scripting))
    print("\n## All tasks\n\n" + table(runs, arms, None))
    print("\n## Per task (passed/runs, mean requests)\n")
    print("| task | " + " | ".join(f"`{arm}`" for arm in arms) + " |\n|---|" + "---|" * len(arms))
    for case in every:
        cells = []
        for arm in arms:
            rows = [row for row in runs if row["harness"] == arm and row["case"] == case]
            if not rows:
                cells.append("")
                continue
            m = metrics(rows)
            cells.append(f"{m['passed']}/{m['runs']}, {m['mean_requests'] if m['mean_requests'] is not None else '—'}")
        print(f"| {case} | " + " | ".join(cells) + " |")
    verdict, reasons = decide(runs, rule, manifest["live"]["pass_noninferiority_delta_pp"])
    print("\n## Rule\n")
    for reason in reasons:
        print(json.dumps({key: (round(value, 3) if isinstance(value, float) else value) for key, value in reason.items()}))
    print(f"\nverdict: {verdict}")


if __name__ == "__main__":
    main()
