#!/usr/bin/env python3
"""Paired live Bash-output budget probe, graded outside each disposable workspace."""

from __future__ import annotations

import argparse
import json
import tomllib
from pathlib import Path

import run


PROMPT = (
    "First call the direct Bash tool once with command `seq 1 100000` and inspect its output. "
    "Do not use run_code for this command. Then fix sum_even in calc.py and run the tests."
)
CASE = {"id": "even_sum", "prompt": PROMPT, "grader": "python3 -m unittest -q"}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    with run.MANIFEST.open("rb") as stream:
        manifest = tomllib.load(stream)
    paths = run.path_map(argparse.Namespace(omp=None, unreal=None))
    rows = []
    for repetition, budgets in enumerate(((15_000, 5_500), (5_500, 15_000))):
        for end_bytes in budgets:
            run.BASE_ENV["AIM_BASH_MODEL_END_BYTES"] = str(end_bytes)
            row = run.run_once("aim_openrouter", CASE, repetition, paths, "live", manifest["live"]["openrouter_model"], None, 180,
                               spend_cap_usd=manifest["live"]["max_spend_usd"],
                               request_reserve_usd=manifest["live"]["max_request_spend_usd"])
            row["bash_model_end_bytes"] = end_bytes
            rows.append(row)
            print(json.dumps({"budget": end_bytes, "rep": repetition, "passed": row["passed"],
                              "output_chars": row["max_tool_output_chars"], "ite": row["ite"],
                              "cost_usd": row["usage"]["cost_usd"]}), flush=True)
            if row["usage"]["cost_usd"] is None or sum(value["usage"]["cost_usd"] or 0 for value in rows) > 0.20:
                raise RuntimeError("output budget probe exceeded its $0.20 spend guard or lost cost accounting")
    result = {"model": manifest["live"]["openrouter_model"], "prompt": PROMPT,
              "aim_sha256": run.executable_hash(paths["aim"]), "runs": rows}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
