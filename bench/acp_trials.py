#!/usr/bin/env python3
"""Code-mode trials for `acp:claude` (T4b, bench/plans/code-mode.md).

Claude Code sends its model requests from the adapter to Anthropic, not through the recording
proxy, so this driver measures what it can see:

- the model's own tool calls: `tool_started` updates in aim's `--json` stream. The code-mode relay
  runs a cell's nested calls without child events (ADR 0076 §4), so nested calls are not visible;
- the turn's token usage, from the ACP `session/prompt` result that aim reports as `usage`;
- model requests, counted as distinct assistant message ids in Claude Code's own session transcript
  for the trial's workspace (`~/.claude/projects/<mangled cwd>/*.jsonl`); only ids and numeric
  usage are read;
- wall time, and the live tier's independent grader.

Each trial gets a fresh workspace and aim home under `run.TRIAL_ROOT`; Claude Code keeps the
caller's HOME, which holds its subscription login. The result keeps numbers and tool names only,
never model text, arguments or credentials. `--max-runs` bounds the subscription runs.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
import tomllib
from collections import Counter
from pathlib import Path

from live_tasks import grade, prepare
from run import MANIFEST, ROOT, executable_hash, source_dirty, temporary_workspace


def binaries() -> dict[str, Path]:
    metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
                              cwd=ROOT, capture_output=True, text=True, check=True)
    debug = Path(json.loads(metadata.stdout)["target_directory"]) / "debug"
    return {"aim": debug / "aim", "aimx": debug / "aimx", "aim_coderun": debug / "aim-coderun"}


def events(stdout: bytes) -> dict:
    """Tool names, usage and the terminal event of one `aim run --json` turn."""
    direct: Counter[str] = Counter()
    nested: Counter[str] = Counter()
    failed: Counter[str] = Counter()
    usage = Counter()
    terminal = None
    for line in stdout.decode(errors="replace").splitlines():
        try:
            update = json.loads(line)
        except ValueError:
            continue
        kind, name = update.get("type"), update.get("name")
        if kind == "tool_started" and isinstance(name, str):
            (nested if update.get("parent") else direct)[name] += 1
        elif kind == "tool_finished" and isinstance(name, str) and (update.get("result") or {}).get("is_error"):
            failed[name] += 1
        elif kind == "usage":
            for field in ("input_tokens", "cached_input_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens"):
                value = (update.get("usage") or {}).get(field)
                if isinstance(value, int):
                    usage[field] += value
        elif kind in {"turn_ended", "turn_failed"}:
            terminal = kind
    return {"direct_tool_calls_by_name": dict(direct), "nested_tool_calls_by_name": dict(nested),
            "failed_tools_by_name": dict(failed), "direct_tool_calls": sum(direct.values()),
            "nested_tool_calls": sum(nested.values()), "usage": dict(usage), "terminal": terminal}


def claude_requests(workspace: Path) -> dict | None:
    """Model requests and their summed usage from Claude Code's transcript of this workspace."""
    folder = Path.home() / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", str(workspace.resolve()))
    ids: set[str] = set()
    usage = Counter()
    for transcript in sorted(folder.glob("*.jsonl")):
        for line in transcript.read_text(errors="replace").splitlines():
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            message = entry.get("message") if entry.get("type") == "assistant" else None
            if not isinstance(message, dict) or not isinstance(message.get("id"), str) or message["id"] in ids:
                continue
            ids.add(message["id"])
            for field in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"):
                value = (message.get("usage") or {}).get(field)
                if isinstance(value, int):
                    usage[field] += value
    return {"requests": len(ids), "usage": dict(usage)} if ids else None


def ite(usage: dict) -> float | None:
    """ADR 0024's input-token equivalents from aim's usage fields (input includes cache reads/writes)."""
    if "input_tokens" not in usage or "output_tokens" not in usage:
        return None
    cached, written = usage.get("cached_input_tokens", 0), usage.get("cache_write_tokens", 0)
    return round(usage["input_tokens"] - cached - written + 0.1 * cached + 1.25 * written + 5 * usage["output_tokens"], 2)


def trial(arm: str, task: dict, repetition: int, model: str, paths: dict[str, Path], timeout: int) -> dict:
    with temporary_workspace() as root:
        workspace = root / "work"
        prepare(task["id"], workspace)
        env = {name: value for name, value in os.environ.items()
               if name not in {"OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY", "TYPESAFE_API_KEY", "AIM_CODE_MODE"}}
        env.update(AIM_HOME=str(root / "aim-home"), AIM_CODE_MODE=arm, AIM_CODERUN=str(paths["aim_coderun"]),
                   AIM_BIN=str(paths["aim"]))
        command = [str(paths["aim"]), "run", "--aimx", str(paths["aimx"]), "-p", "acp:claude", "-m", model,
                   "-C", str(workspace), "--json", task["prompt"]]
        started = time.monotonic()
        timed_out = False
        try:
            process = subprocess.run(command, cwd=workspace, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
            exit_code, stdout = process.returncode, process.stdout
        except subprocess.TimeoutExpired as error:
            timed_out, exit_code, stdout = True, None, error.stdout or b""
        wall_ms = round((time.monotonic() - started) * 1000, 2)
        seen = events(stdout)
        transcript = claude_requests(workspace)
        passed = exit_code == 0 and not timed_out and grade(task["id"], workspace)
    return {"harness": f"acp_claude@{arm}", "case": task["id"], "repetition": repetition, "model": model,
            "exit_code": exit_code, "timed_out": timed_out, "wall_ms": wall_ms, "passed": passed,
            "requests": transcript["requests"] if transcript else None,
            "transcript_usage": transcript["usage"] if transcript else None,
            "ite": ite(seen["usage"]), **seen}


def summary(runs: list[dict], harnesses: list[str]) -> list[dict]:
    result = []
    for harness in harnesses:
        group = [row for row in runs if row["harness"] == harness]
        if not group:
            continue
        passes = sum(row["passed"] for row in group)
        known = [row["ite"] for row in group if row["ite"] is not None]
        requests = [row["requests"] for row in group if row["requests"] is not None]
        result.append({"harness": harness, "runs": len(group), "passed": passes, "pass_rate": round(passes / len(group), 3),
                       "ite_per_passed": round(sum(known) / passes, 2) if passes and len(known) == len(group) else None,
                       "mean_requests": round(statistics.mean(requests), 2) if len(requests) == len(group) else None,
                       "mean_direct_tool_calls": round(statistics.mean(row["direct_tool_calls"] for row in group), 2),
                       "p50_wall_ms": round(statistics.median(row["wall_ms"] for row in group), 2)})
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--arms", default="off,on", help="comma-separated AIM_CODE_MODE values")
    parser.add_argument("--cases", required=True, help="comma-separated live task ids")
    parser.add_argument("--repetitions", type=int, default=2)
    parser.add_argument("--model", help="the manifest's `code_mode.acp_model` by default")
    parser.add_argument("--timeout", type=int, help="seconds; the manifest's `code_mode.acp_timeout_seconds` by default")
    parser.add_argument("--max-runs", type=int, required=True, help="subscription runs this invocation may start")
    args = parser.parse_args()
    with MANIFEST.open("rb") as stream:
        manifest = tomllib.load(stream)
    tasks = {task["id"]: task for task in manifest["live"]["task"]}
    args.model = args.model or manifest["code_mode"]["acp_model"]
    args.timeout = args.timeout or manifest["code_mode"]["acp_timeout_seconds"]
    arms = args.arms.split(",")
    if any(arm not in {"off", "on", "only"} for arm in arms):
        parser.error("arms are off, on or only")
    cases = args.cases.split(",")
    if any(case not in tasks for case in cases):
        parser.error("an unknown task id was selected")
    if len(arms) * len(cases) * args.repetitions > args.max_runs:
        parser.error(f"{len(arms) * len(cases) * args.repetitions} trials exceed --max-runs {args.max_runs}")
    paths = binaries()
    for path in paths.values():
        if not path.is_file():
            parser.error(f"missing binary: {path}")
    runs = []
    started = time.time()
    for case_index, case in enumerate(cases):
        for repetition in range(args.repetitions):
            offset = (case_index + repetition) % len(arms)
            for arm in arms[offset:] + arms[:offset]:
                row = trial(arm, tasks[case], repetition, args.model, paths, args.timeout)
                runs.append(row)
                print(json.dumps({key: row[key] for key in ("harness", "case", "repetition", "passed", "requests",
                                                            "direct_tool_calls", "ite", "wall_ms")}), flush=True)
    harnesses = [f"acp_claude@{arm}" for arm in arms]
    result = {"tier": "acp_live", "manifest_sha256": executable_hash(MANIFEST),
              "source_sha": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True,
                                           text=True).stdout.strip(),
              "source_dirty": source_dirty(),
              "binary_sha256": {name: executable_hash(path) for name, path in paths.items()},
              "started_unix": started, "finished_unix": time.time(), "subscription_runs": len(runs),
              "runs": runs, "summary": summary(runs, harnesses)}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"wrote {args.out}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
