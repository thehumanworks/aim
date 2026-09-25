#!/usr/bin/env python3
"""Scratch driver: run each harness headless against the zero-latency mock.

Per run: fresh HOME, fresh mock server + output dir. Records spawn->first
request (ms), spawn->exit (ms), max RSS (bytes), and the first request's
static-prefix breakdown. Usage: drive.py REPS MODEL [harness...]
"""
import base64, json, os, re, shutil, socket, subprocess, sys, time
from pathlib import Path

SP = Path(__file__).resolve().parent
UNREAL = SP / "bin" / "unreal-agent-runner"
OMP = SP / "omp" / "node_modules" / ".bin" / "omp"
PROMPT = "Reply with OK."


def jwt():
    p = lambda v: base64.urlsafe_b64encode(json.dumps(v).encode()).decode().rstrip("=")
    return ".".join((p({"alg": "none"}), p({"exp": int(time.time()) + 86400,
        "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account", "chatgpt_plan_type": "pro"}}), "fixture"))


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close(); return port


def wait_port(port):
    for _ in range(200):
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.05).close(); return
        except OSError:
            time.sleep(0.02)
    raise RuntimeError("mock did not start")


def invocation(name, home, url, model):
    token = jwt()
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "TERM": "xterm", "LANG": "en_US.UTF-8",
           "XDG_CONFIG_HOME": str(home / ".config"), "XDG_DATA_HOME": str(home / ".local/share"),
           "XDG_CACHE_HOME": str(home / ".cache"), "XDG_STATE_HOME": str(home / ".local/state")}
    if name == "codex":
        (home / ".codex").mkdir(parents=True)
        env.update(CODEX_HOME=str(home / ".codex"), MOCK_KEY="sk-mock")
        return env, ["codex", "exec", "--ignore-user-config", "--ignore-rules", "-m", model,
                     "-c", 'model_reasoning_effort="low"', "-c", 'model_provider="mock"',
                     "-c", f'model_providers.mock={{name="mock",base_url="{url}/v1",wire_api="responses",env_key="MOCK_KEY",supports_websockets=false}}',
                     "-c", 'web_search="disabled"', "--dangerously-bypass-approvals-and-sandbox",
                     "--skip-git-repo-check", "--json", PROMPT]
    if name == "pi":
        ad = home / ".pi" / "agent"; ad.mkdir(parents=True)
        (ad / "models.json").write_text(json.dumps({"providers": {"openai-codex": {"baseUrl": url}}}))
        (ad / "settings.json").write_text(json.dumps({"transport": "sse"}))
        (ad / "auth.json").write_text(json.dumps({"openai-codex": {"type": "oauth", "access": token,
            "refresh": "fixture", "expires": (int(time.time()) + 86400) * 1000}}))
        env.update(PI_CODING_AGENT_DIR=str(ad), PI_OFFLINE="1", PI_SKIP_VERSION_CHECK="1", PI_TELEMETRY="0")
        return env, ["pi", "--provider", "openai-codex", "--model", model, "--thinking", "low",
                     "--mode", "json", "--print", PROMPT]
    if name == "omp":
        ad = home / ".omp" / "agent"; ad.mkdir(parents=True)
        (ad / "models.yml").write_text(f"providers:\n  openai-codex:\n    baseUrl: {url}\n")
        env.update(OPENAI_CODEX_OAUTH_TOKEN=token, PI_CODEX_WEBSOCKET="0", OMP_SKIP_SETUP="1")
        return env, [str(OMP), "--model", f"openai-codex/{model}", "--thinking", "low", "--mode", "json",
                     "--print", "--auto-approve", "--no-title", "--no-skills", "--no-rules", PROMPT]
    if name == "unreal":
        env.update(UNREAL_HARNESS_LLM_PROVIDER="openai-codex", UNREAL_HARNESS_LLM_BASE_URL=url + "/backend-api/codex",
                   UNREAL_HARNESS_LLM_MODEL=model, UNREAL_HARNESS_LLM_MAX_ATTEMPTS="1", OPENAI_CODEX_ACCESS_TOKEN=token)
        return env, [str(UNREAL), json.dumps({"prompt": PROMPT, "model": model, "thinking_level": "low", "max_attempts": 1})]
    raise SystemExit(name)


def run(name, model, rep, root):
    run_dir = root / f"{name}-{model}-{rep}"
    shutil.rmtree(run_dir, ignore_errors=True)
    home, ws, out = run_dir / "home", run_dir / "ws", run_dir / "mock"
    home.mkdir(parents=True); ws.mkdir(); subprocess.run(["git", "init", "-q", str(ws)], check=True)
    port = free_port()
    mock_env = dict(os.environ)
    if os.environ.get("SCENARIO") == "bigout":
        name_args = {"codex": ("exec_command", {"cmd": "seq 1 100000"}),
                     "pi": ("bash", {"command": "seq 1 100000", "timeout": None}),
                     "omp": ("bash", {"i": "count", "command": "seq 1 100000"}),
                     "unreal": ("Bash", {"command": "seq 1 100000"})}[name]
        mock_env.update(MOCK_TOOL_NAME=name_args[0], MOCK_TOOL_ARGS=json.dumps(name_args[1]))
    mock = subprocess.Popen([sys.executable, str(SP / "mock_responses.py"), str(port), str(out)], env=mock_env)
    try:
        wait_port(port)
        env, cmd = invocation(name, home, f"http://127.0.0.1:{port}", model)
        t0 = time.time()
        p = subprocess.run(["/usr/bin/time", "-l", *cmd], cwd=ws, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
        t1 = time.time()
    finally:
        mock.terminate(); mock.wait()
    rss = int(re.search(r"(\d+)\s+maximum resident set size", p.stderr).group(1))
    rows = [json.loads(l) for l in (out / "requests.jsonl").read_text().splitlines()] if (out / "requests.jsonl").exists() else []
    first = rows[0] if rows else {}
    return {"harness": name, "model": model, "rep": rep, "exit": p.returncode,
            "first_request_ms": round((first.get("t_arrival", t1) - t0) * 1000, 1) if rows else None,
            "exit_ms": round((t1 - t0) * 1000, 1), "max_rss_mb": round(rss / 1e6, 1), "requests": len(rows),
            "json_bytes": first.get("json_bytes"), "wire_bytes": first.get("wire_bytes"),
            "stdout_tail": p.stdout[-300:] if p.returncode else "", "stderr_tail": p.stderr[-400:] if p.returncode else ""}


if __name__ == "__main__":
    reps, model, names = int(sys.argv[1]), sys.argv[2], sys.argv[3:]
    root = SP / ("runs-" + os.environ["SCENARIO"] if os.environ.get("SCENARIO") else "runs"); root.mkdir(exist_ok=True)
    for name in names:
        for rep in range(reps):
            print(json.dumps(run(name, model, rep, root)), flush=True)
