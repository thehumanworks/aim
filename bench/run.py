#!/usr/bin/env python3
"""Run the predeclared wire and live cohorts through bench/proxy.py.

All peer commands are resolved before HOME isolation. The child environment carries real keys
only for live runs; recorder artifacts contain numeric usage and hashes, never credentials or
raw model content. No command invokes login or modifies the user's Codex auth file.
"""

from __future__ import annotations

import argparse
import base64
from collections import Counter
import hashlib
import json
import math
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import statistics
import sys
import tempfile
import threading
import time
import tomllib
from contextlib import contextmanager
from pathlib import Path

from live_tasks import grade, graded_files, prepare

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"
MANIFEST = BENCH / "manifest.toml"
PROXY = BENCH / "proxy.py"
CODEX_AUTH = Path.home() / ".codex"
BASE_ENV = os.environ.copy()
# Every trial root lives under this fixed directory, which is also each harness's TMPDIR. Codex CLI,
# pi and aim put the workspace path in model-visible text (pi also names its temp output file), so
# the caller's TMPDIR (`/var/folders/…/T/` on macOS, `/tmp` in some sandboxes) used to change the
# deterministic byte counts by its length (T4b: +44 bytes on every codex and pi row).
TRIAL_ROOT = Path("/tmp")


@contextmanager
def temporary_workspace():
    """Wait briefly for a peer's just-exited helper to stop touching its isolated HOME."""
    path = Path(tempfile.mkdtemp(prefix="aim-bench-", dir=TRIAL_ROOT))
    try:
        yield path
    finally:
        for attempt in range(10):
            try:
                shutil.rmtree(path)
                break
            except OSError:
                if attempt == 9:
                    raise
                time.sleep(0.1 * (attempt + 1))


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
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_port(port: int, process: subprocess.Popen) -> None:
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError("recording proxy exited before listening")
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


def update_diagnostics(stdout: bytes) -> dict:
    """Keep only tool names and failure classes, never model text or tool arguments."""
    calls: Counter[str] = Counter()
    nested: Counter[str] = Counter()
    failed_tools: Counter[str] = Counter()
    patterns: Counter[str] = Counter()
    read_failures: Counter[str] = Counter()
    read_argument_keys: Counter[str] = Counter()
    terminal = None
    for line in stdout.decode(errors="replace").splitlines():
        try:
            update = json.loads(line)
        except ValueError:
            continue
        kind = update.get("type")
        name = update.get("name")
        if kind == "tool_started" and isinstance(name, str):
            calls[name] += 1
            if update.get("parent"):
                nested[name] += 1
            if name == "Read":
                try:
                    arguments = json.loads(update.get("arguments") or "{}")
                    if isinstance(arguments, dict):
                        keys = tuple(sorted(key if key in {"file_path", "offset", "limit"} else "other"
                                            for key in arguments))
                        read_argument_keys[",".join(keys)] += 1
                except (TypeError, ValueError):
                    read_argument_keys["invalid_json"] += 1
        elif kind == "tool_finished" and isinstance(name, str):
            result = update.get("result") or {}
            if result.get("is_error"):
                failed_tools[name] += 1
            content = result.get("content") or []
            text = " ".join(str(part.get("text", "")) for part in content if isinstance(part, dict))[:2048]
            if name == "Read" and result.get("is_error"):
                failure = ("past_end" if "is past the end" in text else
                           "missing_file" if "No such file" in text or "not found" in text or "does not exist" in text else
                           "invalid_arguments" if "arguments" in text or "file_path" in text else
                           "directory" if "is a directory" in text else
                           "denied" if "denied" in text or "outside" in text else "other")
                read_failures[failure] += 1
            for marker in ("SyntaxError", "ReferenceError", "TypeError", "unknown tool", "not found",
                           "is not a function", "Script running", "Traceback", "AssertionError"):
                if marker in text:
                    patterns[marker] += 1
        elif kind == "turn_failed":
            message = str(update.get("message", ""))
            terminal = "max_requests" if "too many" in message or "exceeded" in message else "provider" if "provider" in message else "other"
    # `tool_calls_by_name` counts every call, as before; a nested call is one a code cell made
    # (its update names a `parent`, ADR 0066), so the model's own calls are the difference.
    return {"tool_calls_by_name": dict(calls), "nested_tool_calls_by_name": dict(nested),
            "direct_tool_calls": sum(calls.values()) - sum(nested.values()), "nested_tool_calls": sum(nested.values()),
            "failed_tools_by_name": dict(failed_tools),
            "tool_result_pattern_counts": dict(patterns), "read_failure_classes": dict(read_failures),
            "read_argument_keys": dict(read_argument_keys), "turn_failure_class": terminal}


class ProcessTreeRss:
    """Sample the isolated process group so aimx counts alongside aim."""

    def __init__(self, pgid: int):
        self.pgid = pgid
        self.stop = threading.Event()
        self.peak_bytes = 0
        self.helper_peak_bytes = 0
        self.thread = threading.Thread(target=self.sample, daemon=True)
        self.thread.start()

    def sample(self) -> None:
        while not self.stop.is_set():
            try:
                rows = subprocess.run(["ps", "-axo", "pgid=,rss=,comm="], capture_output=True, text=True,
                                      timeout=2, check=True).stdout.splitlines()
            except (OSError, subprocess.SubprocessError):
                break
            total = helpers = 0
            for row in rows:
                fields = row.split(maxsplit=2)
                if len(fields) != 3 or fields[0] != str(self.pgid):
                    continue
                try:
                    size = int(fields[1]) * 1024
                except ValueError:
                    continue
                total += size
                if Path(fields[2]).name in {"aimx", "aim-coderun"}:
                    helpers += size
            self.peak_bytes = max(self.peak_bytes, total)
            self.helper_peak_bytes = max(self.helper_peak_bytes, helpers)
            self.stop.wait(0.01)

    def finish(self) -> tuple[float | None, float | None]:
        self.stop.set()
        self.thread.join(timeout=3)
        return (round(self.peak_bytes / 1_000_000, 2) if self.peak_bytes else None,
                round(self.helper_peak_bytes / 1_000_000, 2) if self.helper_peak_bytes else None)


def isolated_env(home: Path) -> dict[str, str]:
    env = {name: BASE_ENV[name] for name in ("PATH", "LANG", "LC_ALL", "USER") if name in BASE_ENV}
    env.update(HOME=str(home), TMPDIR=str(TRIAL_ROOT), AIM_HOME=str(home / ".aim"), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_CACHE_HOME=str(home / ".cache"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), TERM="xterm", PYTHONDONTWRITEBYTECODE="1")
    return env


CODE_MODES = ("off", "on", "only")


def split_arm(harness: str) -> tuple[str, str | None]:
    """An aim harness may name its code-mode arm (ADR 0076): `aim_openrouter@only` is
    `aim_openrouter` run with `AIM_CODE_MODE=only`. Other harnesses take no arm."""
    base, separator, arm = harness.partition("@")
    if not separator:
        return harness, None
    if not base.startswith("aim_") or arm not in CODE_MODES:
        raise ValueError(f"unknown harness arm {harness}; aim harnesses take @off, @on or @only")
    return base, arm


def code_mode(arm: str | None) -> str | None:
    """The `AIM_CODE_MODE` an aim run gets: its arm, else the caller's `AIM_CODE_MODE`, else the
    legacy `AIM_BENCH_CODE_MODE=off`; `None` leaves aim's default."""
    if arm is not None:
        return arm
    if BASE_ENV.get("AIM_CODE_MODE"):
        return BASE_ENV["AIM_CODE_MODE"]
    return "off" if BASE_ENV.get("AIM_BENCH_CODE_MODE") == "off" else None


def invocation(harness: str, paths: dict[str, Path], home: Path, url: str, model: str, prompt: str, mode: str,
               effort: str | None, workspace: Path) -> tuple[dict[str, str], list[str]]:
    harness, arm = split_arm(harness)
    env = isolated_env(home)
    env["PATH"] = str(paths["python"].parent) + os.pathsep + env.get("PATH", "")
    if harness.startswith("aim_"):
        env["AIM_CODERUN"] = str(paths["aim_coderun"])
        if (selected := code_mode(arm)) is not None:
            env["AIM_CODE_MODE"] = selected
        if "AIM_BASH_MODEL_END_BYTES" in BASE_ENV:
            env["AIM_BASH_MODEL_END_BYTES"] = BASE_ENV["AIM_BASH_MODEL_END_BYTES"]
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
    metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
                              cwd=ROOT, capture_output=True, text=True, check=True)
    debug = Path(json.loads(metadata.stdout)["target_directory"]) / "debug"
    paths = {"aim": debug / "aim", "aimx": debug / "aimx",
             "aim_coderun": debug / "aim-coderun", "codex": pinned("codex"),
             "pi": pinned("pi"), "python": pinned("python")}
    for name in ("aim", "aimx", "aim_coderun"):
        override = getattr(args, f"{name}_bin", None)
        if override is not None:
            paths[name] = override.resolve()
    if args.omp:
        paths["omp"] = args.omp.resolve()
        paths["bun"] = pinned("bun")
    if args.unreal:
        paths["unreal"] = args.unreal.resolve()
    return paths


def run_once(harness: str, case: dict, repetition: int, paths: dict[str, Path], mode: str, model: str,
             effort: str | None, timeout: int, spend_cap_usd: float | None = None,
             request_reserve_usd: float = 0.0, keep_outputs: Path | None = None) -> dict:
    base, _ = split_arm(harness)
    borrowed_auth = CODEX_AUTH / "auth.json"
    auth_before = executable_hash(borrowed_auth) if base == "aim_codex" and borrowed_auth.exists() else None
    with temporary_workspace() as root:
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
        upstream = "chatgpt.com" if base == "aim_codex" else "openrouter.ai"
        incoming = "/backend-api/codex" if base == "aim_codex" else "/v1"
        upstream_base = "/backend-api/codex" if base == "aim_codex" else "/api/v1"
        recorder = root / "requests.jsonl"
        pgid_file = root / "harness-pgid"
        proxy_command = [sys.executable, "-B", str(PROXY), "--port", str(port), "--out", str(recorder),
                         "--mode", "mock" if mode == "wire" else "live", "--model", model,
                         "--scenario", case.get("scenario", "reply"), "--steps", str(case.get("steps", 0)),
                         "--command", case.get("command", "true"), "--harness", base, "--upstream-host", upstream,
                         "--incoming-base", incoming, "--upstream-base", upstream_base,
                         "--pgid-file", str(pgid_file)]
        if spend_cap_usd is not None:
            proxy_command.extend(["--spend-cap-usd", str(spend_cap_usd), "--request-reserve-usd", str(request_reserve_usd)])
        with (root / "proxy.stderr").open("wb") as proxy_stderr:
            proxy = subprocess.Popen(proxy_command, stdout=subprocess.DEVNULL, stderr=proxy_stderr)
            try:
                wait_port(port, proxy)
                env, command = invocation(harness, paths, home, url, model, case["prompt"], "mock" if mode == "wire" else "live",
                                          effort, workspace)
                start_ns = time.monotonic_ns()
                started = time.monotonic()
                process = subprocess.Popen(time_command(command), cwd=workspace, env=env,
                                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           start_new_session=True)
                pgid_file.write_text(str(process.pid))
                rss = ProcessTreeRss(process.pid)
                timed_out = False
                try:
                    stdout, stderr = process.communicate(timeout=timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    os.killpg(process.pid, signal.SIGKILL)
                    stdout, stderr = process.communicate()
                wall_ms = round((time.monotonic() - started) * 1000, 2)
                tree_rss_mb, helpers_rss_mb = rss.finish()
            finally:
                proxy.terminate()
                try:
                    proxy.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    proxy.kill()
                    proxy.wait()
        all_rows = [json.loads(line) for line in recorder.read_text().splitlines()] if recorder.exists() else []
        rows = [row for row in all_rows if row.get("kind") == "model"]
        append_only_steps = sum(row.get("append_only_with_previous") is True for row in rows[1:])
        stable_head_ratios = [row["stable_head_bytes_with_previous"] / row["stable_head_previous_bytes"]
                              for row in rows[1:] if row.get("stable_head_previous_bytes")]
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
        harness_rss_mb = peak_rss(stderr.decode(errors="replace"))
        peak_tree_mb = max((value for value in (tree_rss_mb, harness_rss_mb, *(row.get("process_group_rss_mb") for row in rows))
                            if value is not None), default=None)
        helpers_rss_mb = max((value for value in (helpers_rss_mb, *(row.get("helpers_rss_mb") for row in rows))
                              if value is not None), default=None)
        result = {"harness": harness, "case": case["id"], "repetition": repetition, "model": model,
                  "effort": effort, "exit_code": process.returncode, "timed_out": timed_out, "wall_ms": wall_ms,
                  "peak_rss_mb": peak_tree_mb, "harness_peak_rss_mb": harness_rss_mb,
                  "helpers_peak_rss_mb": helpers_rss_mb, "requests": len(rows),
                  "auxiliary_requests": len(all_rows) - len(rows), "http_exchanges": len(all_rows),
                  "first_request_ms": round((rows[0]["arrival_monotonic_ns"] - start_ns) / 1e6, 2) if rows else None,
                  "first_request_bytes": rows[0]["request_json_bytes"] if rows else None,
                  "first_request_tokens_estimate": rows[0]["request_tokens_estimate"] if rows else None,
                  "first_request_tools": rows[0]["tools_count"] if rows else None,
                  "first_request_tools_json_bytes": rows[0]["tools_json_bytes"] if rows else None,
                  "first_request_tool_schema_bytes": rows[0]["tool_schema_bytes"] if rows else [],
                  "first_request_instructions_chars": rows[0]["instructions_chars"] if rows else None,
                  "first_token_ms": rows[0].get("first_token_ms") if rows else None,
                  "request_lcp_bytes": [row["lcp_bytes_with_previous"] for row in rows[1:]],
                  "append_only_steps": append_only_steps,
                  "stable_head_ratios": stable_head_ratios,
                  "stable_head_ratio_median": statistics.median(stable_head_ratios) if stable_head_ratios else None,
                  "request_json_bytes": [row["request_json_bytes"] for row in rows],
                  "max_tool_output_chars": max((row["tool_output_chars"] for row in rows), default=0),
                  "tool_output_recovery_hint": any(row["tool_output_has_recovery_hint"] for row in rows),
                  "provider_usage_complete": usage_complete, "usage": usage,
                  "request_usage": [row.get("usage", {}) for row in rows],
                  "provider_cost_complete": cost_complete,
                  "missing_success_cost": missing_success_cost,
                  "transport_errors": len(transport_errors),
                  "client_disconnects_with_usage": sum(row.get("client_disconnected") is True and bool(row.get("usage")) for row in rows),
                  "proxy_spend_cap_hit": any(row.get("spend_cap_hit") is True for row in rows),
                  "proxy_error_types": [row.get("proxy_error_type") for row in transport_errors],
                  "ite": ite, "request_response_sizes": [[row["request_wire_bytes"], row["response_bytes"]] for row in rows],
                  "response_statuses": [row["status"] for row in rows]}
        result.update(update_diagnostics(stdout))
        if base == "aim_codex":
            result["borrowed_codex_auth_unchanged"] = auth_before is not None and executable_hash(borrowed_auth) == auth_before
        if mode == "live":
            result["passed"] = process.returncode == 0 and not timed_out and grade(case["id"], workspace)
            if keep_outputs is not None:
                keep(keep_outputs / harness / f"{case['id']}-r{repetition}", case["id"], workspace)
        else:
            result["passed"] = process.returncode == 0 and len(rows) >= case.get("steps", 0) + 1
        return result


def source_dirty() -> bool:
    """Whether the checkout that ran has uncommitted changes, so `source_sha` alone does not name
    the code (T4b's smoke 2 and 3 ran fixes that were committed afterwards)."""
    status = subprocess.run(["git", "status", "--porcelain"], cwd=ROOT, check=True, capture_output=True, text=True)
    return bool(status.stdout.strip())


def keep(target: Path, task_id: str, workspace: Path) -> None:
    """Copy the files the grader reads, when present, for a human to check a grade."""
    for name in graded_files(task_id):
        source = workspace / name
        if source.is_file() and not source.is_symlink() and source.stat().st_size <= 1_000_000:
            (target / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target / name)


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
                       "mean_requests": round(statistics.mean(row.get("requests", 0) for row in group), 2),
                       "mean_direct_tool_calls": round(statistics.mean(row.get("direct_tool_calls", 0) for row in group), 2),
                       "mean_nested_tool_calls": round(statistics.mean(row.get("nested_tool_calls", 0) for row in group), 2),
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
    parser.add_argument("--aim-bin", type=Path, help="measured aim executable (e.g. the baseline build)")
    parser.add_argument("--aimx-bin", type=Path, help="measured aimx executable")
    parser.add_argument("--aim-coderun-bin", type=Path, help="measured code worker executable")
    parser.add_argument("--source-sha", help="source commit of an explicitly supplied baseline binary")
    parser.add_argument("--no-gate", action="store_true", help="record a diagnostic or immutable baseline without comparison")
    parser.add_argument("--keep-outputs", type=Path, help="live: copy each trial's graded files here to diagnose a grade "
                        "(model-written text: keep it out of committed results)")
    args = parser.parse_args()
    if args.source_sha and (not re.fullmatch(r"[0-9a-f]{40}", args.source_sha)
                            or any(getattr(args, f"{name}_bin") is None for name in ("aim", "aimx", "aim_coderun"))):
        parser.error("--source-sha needs a 40-character hash and all three aim executable overrides")
    if args.tier == "wire" and not args.no_gate and code_mode(None) is not None:
        parser.error("the wire gate measures aim's default code mode: unset AIM_CODE_MODE and AIM_BENCH_CODE_MODE, "
                     "or pass --no-gate")
    with MANIFEST.open("rb") as stream:
        manifest = tomllib.load(stream)
    tier = manifest[args.tier]
    harnesses = args.harnesses.split(",") if args.harnesses else tier["harnesses"]
    paths = path_map(args)
    try:
        bases = [split_arm(name)[0] for name in harnesses]
    except ValueError as error:
        parser.error(str(error))
    missing = [name for name in bases if name.split("_")[0] not in paths and name not in {"aim_openrouter", "aim_codex", "codex_openrouter"}]
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
                base, _ = split_arm(harness)
                reserve = tier.get("max_request_spend_usd", 0.0) if args.tier == "live" and base != "aim_codex" else 0.0
                if args.tier == "live" and base != "aim_codex" and budget_used + reserve > tier["max_spend_usd"]:
                    break
                if base == "aim_codex" and args.tier == "live":
                    if subscription_runs >= tier["max_subscription_runs"]:
                        continue
                    subscription_runs += 1
                model = tier["model"] if args.tier == "wire" else tier["codex_model"] if base == "aim_codex" else tier["openrouter_model"]
                effort = tier["effort"] if args.tier == "wire" else tier["codex_effort"] if base == "aim_codex" else None
                remaining = tier["max_spend_usd"] - budget_used if args.tier == "live" and base != "aim_codex" else None
                row = run_once(harness, case, repetition, paths, args.tier, model, effort, tier.get("timeout_seconds", 90),
                               spend_cap_usd=remaining, request_reserve_usd=reserve, keep_outputs=args.keep_outputs)
                runs.append(row)
                if args.tier == "live" and base != "aim_codex":
                    if row["missing_success_cost"]:
                        budget_used = tier["max_spend_usd"]
                    else:
                        reported_spend += row["usage"]["cost_usd"]
                        budget_used += row["usage"]["cost_usd"] + row["transport_errors"] * tier["unpriced_transport_reserve_usd"]
                print(json.dumps({"harness": harness, "case": case["id"], "rep": repetition,
                                  "passed": row["passed"], "exit": row["exit_code"], "requests": row["requests"],
                                  "ite": row["ite"], "cost_usd": row["usage"]["cost_usd"]}), flush=True)
            if args.tier == "live" and budget_used + tier.get("max_request_spend_usd", 0.0) > tier["max_spend_usd"]:
                break
        if args.tier == "live" and budget_used + tier.get("max_request_spend_usd", 0.0) > tier["max_spend_usd"]:
            break
    result = {"tier": args.tier, "manifest_sha256": executable_hash(MANIFEST), "source_sha": args.source_sha or subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip(),
        "source_dirty": None if args.source_sha else source_dirty(),
        "hardware": {"platform": platform.platform(), "machine": platform.machine(), "python": platform.python_version()},
        "started_unix": started, "finished_unix": time.time(), "binary_sha256": {name: executable_hash(path) for name, path in paths.items()},
        "spend_cap_usd": tier.get("max_spend_usd"), "recorded_spend_usd": round(reported_spend, 6),
        "budget_used_including_transport_reserve_usd": round(budget_used, 6),
        "prewarm": prewarm, "runs": runs, "summary": summary(runs, harnesses)}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"wrote {args.out}", flush=True)
    if args.tier == "wire" and not args.no_gate:
        from compare import compare_wire
        baseline = json.loads((BENCH / manifest["wire"]["baseline"]).read_text())
        errors = compare_wire(result, baseline, manifest)
        if errors:
            for error in errors:
                print(f"wire regression: {error}", file=sys.stderr)
            raise SystemExit(1)
        print("wire gate passed", flush=True)


if __name__ == "__main__":
    main()
