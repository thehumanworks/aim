#!/usr/bin/env python3
"""Deterministic wire gate against the committed current-main baseline.

Latency and sampled RSS are recorded, never gated. Intentional wire-budget changes require an
explicit manifest threshold change, visible in review.
"""

from __future__ import annotations

import argparse
import json
import tomllib
from pathlib import Path


DETERMINISTIC = (
    "first_request_bytes", "first_request_tools", "first_request_instructions_chars",
    "auxiliary_requests", "requests", "max_tool_output_chars", "request_json_bytes",
    "append_only_steps",
)


def groups(result: dict) -> dict[tuple[str, str], list[dict]]:
    output: dict[tuple[str, str], list[dict]] = {}
    for row in result["runs"]:
        output.setdefault((row["harness"], row["case"]), []).append(row)
    return output


def compare_wire(candidate: dict, baseline: dict, manifest: dict) -> list[str]:
    """Return every gate failure; an empty list means the deterministic contract holds."""
    errors: list[str] = []
    if candidate.get("tier") != "wire" or baseline.get("tier") != "wire":
        return ["both artifacts must be wire-tier results"]
    current, old = groups(candidate), groups(baseline)
    if current.keys() != old.keys():
        errors.append("candidate case/harness set differs from the committed baseline")
    wire = manifest["wire"]
    for key in sorted(old.keys() | current.keys()):
        measured, reference = current.get(key, []), old.get(key, [])
        if len(measured) != wire["repetitions"] or len(reference) != wire["repetitions"]:
            errors.append(f"{key}: expected {wire['repetitions']} repetitions in both artifacts")
            continue
        measured.sort(key=lambda row: row["repetition"])
        reference.sort(key=lambda row: row["repetition"])
        if any(not row["passed"] for row in measured):
            errors.append(f"{key}: a scripted trajectory failed")
        for field in DETERMINISTIC:
            if measured[0].get(field) != measured[1].get(field):
                errors.append(f"{key}: repetitions disagree on {field}")
        row, base = measured[0], reference[0]
        harness, case = key
        if row["requests"] != base["requests"]:
            errors.append(f"{key}: model request count changed")
        if row["first_request_instructions_chars"] != base["first_request_instructions_chars"]:
            errors.append(f"{key}: instruction size changed without an acknowledged baseline")
        expected_tools = wire["aim_expected_tools"] if harness == "aim_openrouter" else base["first_request_tools"]
        if row["first_request_tools"] != expected_tools:
            errors.append(f"{key}: expected {expected_tools} advertised tools")
        max_auxiliary = wire["aim_max_auxiliary_requests"] if harness == "aim_openrouter" else base["auxiliary_requests"]
        if row["auxiliary_requests"] > max_auxiliary:
            errors.append(f"{key}: extra auxiliary requests before or during the turn")
        if row["first_request_bytes"] > base["first_request_bytes"]:
            errors.append(f"{key}: first request grew beyond current-main baseline")
        if case == "W1" and harness == "aim_openrouter" and row["first_request_bytes"] > wire["aim_w1_max_bytes"]:
            errors.append(f"{key}: first request exceeds the manifest's W1 budget")
        if len(row["request_json_bytes"]) != len(base["request_json_bytes"]) or any(
            current_size > old_size for current_size, old_size in zip(row["request_json_bytes"], base["request_json_bytes"])
        ):
            errors.append(f"{key}: a request grew beyond the current-main baseline")
        if case in {"W2", "W3"} and row["max_tool_output_chars"] > base["max_tool_output_chars"]:
            errors.append(f"{key}: tool output exposure grew")
        if case == "W2" and harness == "aim_openrouter":
            if row["max_tool_output_chars"] > wire["aim_w2_max_tool_output_chars"]:
                errors.append(f"{key}: Bash output exceeds the W26 model-view budget")
            if not row["tool_output_recovery_hint"]:
                errors.append(f"{key}: the full-output recovery hint is missing")
        if row["append_only_steps"] != base["append_only_steps"] or row["append_only_steps"] != row["requests"] - 1:
            errors.append(f"{key}: prompt stopped being append-only in provider render order")
        if case == "W4" and row.get("stable_head_ratio_median") != 1.0:
            errors.append(f"{key}: provider-rendered stable prefix is incomplete")
    return errors


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    with args.manifest.open("rb") as stream:
        manifest = tomllib.load(stream)
    errors = compare_wire(json.loads(args.candidate.read_text()), json.loads(args.baseline.read_text()), manifest)
    for error in errors:
        print(error)
    if errors:
        raise SystemExit(1)
    print("wire gate passed")


if __name__ == "__main__":
    main()
