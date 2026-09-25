#!/usr/bin/env python3
"""Run the predeclared wire and live cohorts through bench/proxy.py.

All peer commands are resolved before HOME isolation. The child environment carries real keys
only for live runs; recorder artifacts contain numeric usage and hashes, never credentials or
raw model content. No command invokes login or modifies the user's Codex auth file.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import math
import os
import platform
import re
import signal
import socket
import subprocess
import statistics
import sys
import tempfile
import time
import tomllib
from pathlib import Path

from live_tasks import grade, prepare
from port import fixed_port

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"
MANIFEST = BENCH / "manifest.toml"
PROXY = BENCH / "proxy.py"
CODEX_AUTH = Path.home() / ".codex"
BASE_ENV = os.environ.copy()


def pinned(name: str) -> Path:
    result = subprocess.run(["mise", "which", name], cwd=ROOT, capture_output=True, text=True, check=True)
    return Path(result.stdout.strip()).resolve()


def executable_hash(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def fixture_jwt() -> str:
    def component(value: dict) -> str:
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")
    return ".".join((component({"alg": "none"}), component({"exp": int(time.time()) + 86400,
        "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account", "chatgpt_plan_type": "pro"}}), "fixture"))


def free_port() -> int:
    selected = fixed_port(os.environ.get("AIM_GATE_BENCH_PORT"))
    if selected is not None:
        return selected
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_port(port: int, process: subprocess.Popen, ready: Path) -> None:
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError("recording proxy exited before listening")
        try:
            if int(ready.read_text()) != process.pid:
                raise RuntimeError("recording proxy readiness came from another process")
        except FileNotFoundError:
            time.sleep(0.02)
            continue
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.05):
                return
        except OSError:
            time.sleep(0.02)
    raise TimeoutError("recording proxy did not listen")


def time_command(binary: list[str]) -> list[str]:
    if Path("/usr/bin/time").exists() and platform.system() == "Darwin":
        return ["/usr/bin/time", "-l", *binary]
    if Path("/usr/bin/time").exists() and platform.system() == "Linux":
        return ["/usr/bin/time", "-v", *binary]
    return binary


def peak_rss(stderr: str) -> float | None:
    mac = re.search(r"(\d+)\s+maximum resident set size", stderr)
    linux = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", stderr)
    if mac:
        return round(int(mac.group(1)) / 1_000_000, 2)
    if linux:
        return round(int(linux.group(1)) * 1024 / 1_000_000, 2)
    return None


def isolated_env(home: Path) -> dict[str, str]:
    env = {name: BASE_ENV[name] for name in ("PATH", "LANG", "LC_ALL", "TMPDIR", "USER") if name in BASE_ENV}
    env.update(HOME=str(home), AIM_HOME=str(home / ".aim"), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_CACHE_HOME=str(home / ".cache"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), TERM="xterm", PYTHONDONTWRITEBYTECODE="1")
    return env


def invocation(harness: str, paths: dict[str, Path], home: Path, url: str, model: str, prompt: str, mode: str,
               effort: str | None, workspace: Path) -> tuple[dict[str, str], list[str]]:
    env = isolated_env(home)
    env["PATH"] = str(paths["python"].parent) + os.pathsep + env.get("PATH", "")
    if harness.startswith("aim_"):
        env["AIM_CODERUN"] = str(paths["aim_coderun"])
    if harness == "aim_openrouter":
        env["AIM_OPENROUTER_BASE_URL"] = url + "/v1"
        env["OPENROUTER_API_KEY"] = "fixture" if mode == "mock" else BASE_ENV["OPENROUTER_API_KEY"]
        args = [str(paths["aim"]), "run", "--ephemeral", "--aimx", str(paths["aimx"]),
                "-p", "openrouter", "-m", model, "-C", str(workspace), "--json", "--max-requests", "24"]
        if effort:
            args.extend(["-e", effort])
        return env, [*args, prompt]
    if harness == "aim_codex":
        env["AIM_CODEX_BASE_URL"] = url + "/backend-api/codex"
        env["CODEX_HOME"] = str(CODEX_AUTH)
        return env, [str(paths["aim"]), "run", "--ephemeral", "--aimx", str(paths["aimx"]),
                     "-p", "codex", "-m", model, "-e", effort or "low", "-C", str(workspace),
                     "--json", "--max-requests", "24", prompt]
    if harness in {"codex", "codex_openrouter"}:
        codex_home = home / ".codex"
        codex_home.mkdir(parents=True)
        env["CODEX_HOME"] = str(codex_home)
        if mode == "mock":
            env["BENCH_PROXY_KEY"] = "fixture"
        else:
            env["BENCH_PROXY_KEY"] = BASE_ENV["OPENROUTER_API_KEY"]
        provider = (f'model_providers.bench={{name="bench",base_url="{url}/v1",wire_api="responses",'
                    'env_key="BENCH_PROXY_KEY",supports_websockets=false}')
        args = [str(paths["codex"]), "exec", "--ignore-user-config", "--ignore-rules", "-m", model,
                "-c", 'model_provider="bench"', "-c", provider, "-c", 'web_search="disabled"',
                "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check", "--json"]
        if effort:
            args.extend(["-c", f'model_reasoning_effort="{effort}"'])
        return env, [*args, prompt]
    if harness == "pi":
        agent = home / ".pi/agent"
        agent.mkdir(parents=True)
        (agent / "models.json").write_text(json.dumps({"providers": {"openai-codex": {"baseUrl": url}}}))
        (agent / "settings.json").write_text(json.dumps({"transport": "sse"}))
        (agent / "auth.json").write_text(json.dumps({"openai-codex": {"type": "oauth", "access": fixture_jwt(),
                            "refresh": "fixture", "expires": (int(time.time()) + 86400) * 1000}}))
        env.update(PI_CODING_AGENT_DIR=str(agent), PI_OFFLINE="1", PI_SKIP_VERSION_CHECK="1", PI_TELEMETRY="0")
        return env, [str(paths["pi"]), "--provider", "openai-codex", "--model", model,
                     "--thinking", effort or "low", "--mode", "json", "--print", prompt]
    if harness == "omp":
        agent = home / ".omp/agent"
        agent.mkdir(parents=True)
        (agent / "models.yml").write_text(f"providers:\n  openai-codex:\n    baseUrl: {url}\n")
        env.update(OPENAI_CODEX_OAUTH_TOKEN=fixture_jwt(), PI_CODEX_WEBSOCKET="0", OMP_SKIP_SETUP="1")
        env["PATH"] = str(paths["bun"].parent) + os.pathsep + env["PATH"]
        return env, [str(paths["omp"]), "--model", f"openai-codex/{model}", "--thinking", effort or "low",
                     "--mode", "json", "--print", "--auto-approve", "--no-title", "--no-skills", "--no-rules", prompt]
    if harness == "unreal":
        env.update(UNREAL_HARNESS_LLM_PROVIDER="openai-codex", UNREAL_HARNESS_LLM_BASE_URL=url + "/backend-api/codex",
                   UNREAL_HARNESS_LLM_MODEL=model, UNREAL_HARNESS_LLM_MAX_ATTEMPTS="1", OPENAI_CODEX_ACCESS_TOKEN=fixture_jwt())
        return env, [str(paths["unreal"]), json.dumps({"prompt": prompt, "model": model, "thinking_level": effort or "low", "max_attempts": 1})]
    raise ValueError(f"unknown harness {harness}")


def path_map(args: argparse.Namespace) -> dict[str, Path]:
    paths = {"aim": ROOT / "target/debug/aim", "aimx": ROOT / "target/debug/aimx",
             "aim_coderun": ROOT / "target/debug/aim-coderun", "codex": pinned("codex"),
             "pi": pinned("pi"), "python": pinned("python")}
    if args.omp:
        paths["omp"] = args.omp.resolve()
        paths["bun"] = pinned("bun")
    if args.unreal:
        paths["unreal"] = args.unreal.resolve()
    return paths


def run_once(harness: str, case: dict, repetition: int, paths: dict[str, Path], mode: str, model: str,
             effort: str | None, timeout: int) -> dict:
    borrowed_auth = CODEX_AUTH / "auth.json"
    auth_before = executable_hash(borrowed_auth) if harness == "aim_codex" and borrowed_auth.exists() else None
    with tempfile.TemporaryDirectory(prefix="aim-bench-") as temp:
        root = Path(temp)
        home, workspace = root / "home", root / "work"
        home.mkdir()
        if mode == "live":
            prepare(case["id"], workspace)
        else:
            workspace.mkdir()
            subprocess.run(["git", "init", "-q", str(workspace)], check=True)
            if case["scenario"] == "big_file":
                (workspace / "big.txt").write_text("".join(f"{line:05d} benchmark line\n" for line in range(5000)))
        port = free_port()
        url = f"http://127.0.0.1:{port}"
        upstream = "chatgpt.com" if harness == "aim_codex" else "openrouter.ai"
        incoming = "/backend-api/codex" if harness == "aim_codex" else "/v1"
        upstream_base = "/backend-api/codex" if harness == "aim_codex" else "/api/v1"
        recorder = root / "requests.jsonl"
        ready = root / "proxy.ready"
        proxy_command = [sys.executable, "-B", str(PROXY), "--port", str(port), "--out", str(recorder),
                         "--ready-file", str(ready),
                         "--mode", "mock" if mode == "wire" else "live", "--model", model,
                         "--scenario", case.get("scenario", "reply"), "--steps", str(case.get("steps", 0)),
                         "--command", case.get("command", "true"), "--harness", harness, "--upstream-host", upstream,
                         "--incoming-base", incoming, "--upstream-base", upstream_base]
        with (root / "proxy.stderr").open("wb") as proxy_stderr:
            proxy = subprocess.Popen(proxy_command, stdout=subprocess.DEVNULL, stderr=proxy_stderr)
            try:
                wait_port(port, proxy, ready)
                env, command = invocation(harness, paths, home, url, model, case["prompt"], "mock" if mode == "wire" else "live",
                                          effort, workspace)
                start_ns = time.time_ns()
                started = time.monotonic()
                process = subprocess.Popen(time_command(command), cwd=workspace, env=env,
                                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           start_new_session=True)
                timed_out = False
                try:
                    stdout, stderr = process.communicate(timeout=timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    os.killpg(process.pid, signal.SIGKILL)
                    stdout, stderr = process.communicate()
                wall_ms = round((time.monotonic() - started) * 1000, 2)
            finally:
                proxy.terminate()
                try:
                    proxy.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    proxy.kill()
                    proxy.wait()
        all_rows = [json.loads(line) for line in recorder.read_text().splitlines()] if recorder.exists() else []
        rows = [row for row in all_rows if row.get("kind") == "model"]
        usage = {field: sum(row.get("usage", {}).get(field, 0) for row in rows) for field in
                 ("input_tokens", "cached_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens", "cost_usd")}
        transport_errors = [row for row in rows if row.get("status") != 200]
        missing_success_usage = any(row.get("status") == 200 and
                                    ("input_tokens" not in row.get("usage", {}) or "output_tokens" not in row.get("usage", {})) for row in rows)
        missing_success_cost = any(row.get("status") == 200 and "cost_usd" not in row.get("usage", {}) for row in rows)
        usage_complete = bool(rows) and not missing_success_usage and not transport_errors
        cost_complete = bool(rows) and not missing_success_cost and not transport_errors
        if missing_success_cost:
            usage["cost_usd"] = None
        ite = None if not usage_complete else round(usage["input_tokens"] - usage["cached_tokens"] - usage["cache_write_tokens"]
               + 0.1 * usage["cached_tokens"] + 1.25 * usage["cache_write_tokens"] + 5 * usage["output_tokens"], 2)
        result = {"harness": harness, "case": case["id"], "repetition": repetition, "model": model,
                  "effort": effort, "exit_code": process.returncode, "timed_out": timed_out, "wall_ms": wall_ms,
                  "peak_rss_mb": peak_rss(stderr.decode(errors="replace")), "requests": len(rows),
                  "auxiliary_requests": len(all_rows) - len(rows), "http_exchanges": len(all_rows),
                  "first_request_ms": round((rows[0]["arrival_wall_ns"] - start_ns) / 1e6, 2) if rows else None,
                  "first_request_bytes": rows[0]["request_json_bytes"] if rows else None,
                  "first_request_tokens_estimate": rows[0]["request_tokens_estimate"] if rows else None,
                  "first_request_tools": rows[0]["tools_count"] if rows else None,
                  "first_request_instructions_chars": rows[0]["instructions_chars"] if rows else None,
                  "first_token_ms": rows[0].get("first_token_ms") if rows else None,
                  "request_lcp_bytes": [row["lcp_bytes_with_previous"] for row in rows[1:]],
                  "request_json_bytes": [row["request_json_bytes"] for row in rows],
                  "max_tool_output_chars": max((row["tool_output_chars"] for row in rows), default=0),
                  "tool_output_recovery_hint": any(row["tool_output_has_recovery_hint"] for row in rows),
                  "provider_usage_complete": usage_complete, "usage": usage,
                  "provider_cost_complete": cost_complete,
                  "missing_success_cost": missing_success_cost,
                  "transport_errors": len(transport_errors),
                  "proxy_error_types": [row.get("proxy_error_type") for row in transport_errors],
                  "ite": ite, "request_response_sizes": [[row["request_wire_bytes"], row["response_bytes"]] for row in rows],
                  "response_statuses": [row["status"] for row in rows]}
        if harness == "aim_codex":
            result["borrowed_codex_auth_unchanged"] = auth_before is not None and executable_hash(borrowed_auth) == auth_before
        if mode == "live":
            result["passed"] = process.returncode == 0 and not timed_out and grade(case["id"], workspace)
        else:
            result["passed"] = process.returncode == 0 and len(rows) >= case.get("steps", 0) + 1
        return result


def summary(runs: list[dict], harnesses: list[str]) -> list[dict]:
    result = []
    for harness in harnesses:
        group = [row for row in runs if row["harness"] == harness]
        if not group:
            continue
        passes = sum(row["passed"] for row in group)
        total_ite = sum(row["ite"] for row in group if row["ite"] is not None)
        complete_ite = all(row["ite"] is not None for row in group)
        measured_cost = all(row["provider_cost_complete"] for row in group)
        total_usd = sum(row["usage"]["cost_usd"] for row in group if row["usage"]["cost_usd"] is not None)
        n = len(group)
        z = 1.96
        fraction = passes / n
        denominator = 1 + z * z / n
        center = (fraction + z * z / (2 * n)) / denominator
        radius = z * math.sqrt(fraction * (1 - fraction) / n + z * z / (4 * n * n)) / denominator
        result.append({"harness": harness, "runs": len(group), "passed": passes, "pass_rate": round(passes / len(group), 3),
                       "pass_rate_ci95": [round(center - radius, 3), round(center + radius, 3)],
                       "ite_per_passed": round(total_ite / passes, 2) if passes and complete_ite else None,
                       "usd_per_passed": round(total_usd / passes, 6) if passes and measured_cost else None,
                       "p50_wall_ms": round(statistics.median(row["wall_ms"] for row in group), 2)})
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("tier", choices=["wire", "live"])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--repetitions", type=int)
    parser.add_argument("--harnesses", help="comma-separated override of the manifest cohort")
    parser.add_argument("--cases", help="comma-separated case/task ids for diagnosis")
    parser.add_argument("--omp", type=Path, help="local pinned oh-my-pi artifact")
    parser.add_argument("--unreal", type=Path, help="local pinned unreal-agent artifact")
    args = parser.parse_args()
    with MANIFEST.open("rb") as stream:
        manifest = tomllib.load(stream)
    tier = manifest[args.tier]
    harnesses = args.harnesses.split(",") if args.harnesses else tier["harnesses"]
    paths = path_map(args)
    missing = [name for name in harnesses if name.split("_")[0] not in paths and name not in {"aim_openrouter", "aim_codex", "codex_openrouter"}]
    if missing:
        parser.error(f"missing pinned peer artifacts for: {', '.join(missing)}")
    for path in paths.values():
        if not path.is_file():
            parser.error(f"missing binary: {path}")
    if args.tier == "live" and "OPENROUTER_API_KEY" not in BASE_ENV:
        parser.error("OPENROUTER_API_KEY is required for live OpenRouter runs")
    repetitions = args.repetitions or tier["repetitions"]
    cases = tier["case"] if args.tier == "wire" else tier["task"]
    if args.cases:
        selected = set(args.cases.split(","))
        cases = [case for case in cases if case["id"] in selected]
        if len(cases) != len(selected):
            parser.error("an unknown case/task id was selected")
    prewarm = []
    warm_case = manifest["wire"]["case"][0]
    for harness in harnesses:
        for _ in range(tier.get("prewarm_mock_runs", 0)):
            row = run_once(harness, warm_case, -1, paths, "wire", manifest["wire"]["model"], manifest["wire"]["effort"], 90)
            prewarm.append(row)
            if not row["passed"]:
                raise RuntimeError(f"mock prewarm failed for {harness}")
            print(json.dumps({"prewarm": harness, "first_request_ms": row["first_request_ms"]}), flush=True)
    runs = []
    reported_spend = 0.0
    budget_used = 0.0
    subscription_runs = 0
    started = time.time()
    for case_index, case in enumerate(cases):
        for repetition in range(repetitions):
            rotated = harnesses[(case_index + repetition) % len(harnesses):] + harnesses[:(case_index + repetition) % len(harnesses)]
            for harness in rotated:
                if args.tier == "live" and budget_used >= tier["max_spend_usd"]:
                    break
                if harness == "aim_codex" and args.tier == "live":
                    if subscription_runs >= tier["max_subscription_runs"]:
                        continue
                    subscription_runs += 1
                model = tier["model"] if args.tier == "wire" else tier["codex_model"] if harness == "aim_codex" else tier["openrouter_model"]
                effort = tier["effort"] if args.tier == "wire" else tier["codex_effort"] if harness == "aim_codex" else None
                row = run_once(harness, case, repetition, paths, args.tier, model, effort, tier.get("timeout_seconds", 90))
                runs.append(row)
                if args.tier == "live" and harness != "aim_codex":
                    if row["missing_success_cost"]:
                        budget_used = tier["max_spend_usd"]
                    else:
                        reported_spend += row["usage"]["cost_usd"]
                        budget_used += row["usage"]["cost_usd"] + row["transport_errors"] * tier["unpriced_transport_reserve_usd"]
                print(json.dumps({"harness": harness, "case": case["id"], "rep": repetition,
                                  "passed": row["passed"], "exit": row["exit_code"], "requests": row["requests"],
                                  "ite": row["ite"], "cost_usd": row["usage"]["cost_usd"]}), flush=True)
            if args.tier == "live" and budget_used >= tier["max_spend_usd"]:
                break
        if args.tier == "live" and budget_used >= tier["max_spend_usd"]:
            break
    result = {"tier": args.tier, "manifest_sha256": executable_hash(MANIFEST), "source_sha": subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip(),
        "hardware": {"platform": platform.platform(), "machine": platform.machine(), "python": platform.python_version()},
        "started_unix": started, "finished_unix": time.time(), "binary_sha256": {name: executable_hash(path) for name, path in paths.items()},
        "spend_cap_usd": tier.get("max_spend_usd"), "recorded_spend_usd": round(reported_spend, 6),
        "budget_used_including_transport_reserve_usd": round(budget_used, 6),
        "prewarm": prewarm, "runs": runs, "summary": summary(runs, harnesses)}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"wrote {args.out}", flush=True)


if __name__ == "__main__":
    main()
