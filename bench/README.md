# Benchmark harness

`manifest.toml` fixes the cohort before results are collected (ADR 0024). `proxy.py` serves
scripted Responses and Chat Completions SSE for the offline wire tier, or forwards to the real
provider over HTTPS for the live tier. The loopback endpoint is selected through aim's
`AIM_OPENROUTER_BASE_URL` / `AIM_CODEX_BASE_URL` and each peer's own base URL setting. No TLS
interception or change to system trust is needed.

## Run

```sh
mise install
mise run bench:wire
mise run bench:live  # manual, requires OPENROUTER_API_KEY and read-only Codex auth
```

The wire task needs no credentials. It runs aim, pinned Codex CLI and pinned pi with two
repetitions of W1 (answer), W2 (589 KB shell output), W3 (5,000-line file via shell), and W4
(20 serial tool steps). To include locally built oh-my-pi and Unreal Agent artifacts, pass their
paths explicitly. The W22 optional artifacts came from the read-only references at
oh-my-pi `4a7b586821a4df0afcea657717247f6ec9db8f88` and Unreal Agent
`1b9f778453f411c029b39b85102aaefb95e7e48d`; the result records executable hashes:

```sh
python3 -B bench/run.py wire --out bench/history/all-peers.json \
  --harnesses aim_openrouter,codex,pi,omp,unreal \
  --omp /path/to/pinned/omp --unreal /path/to/pinned/unreal-agent-runner
```

The live task creates six small throwaway git repositories per repetition, rotates harness
order, runs independent Python graders, and uses the same OpenRouter model/upstream for aim
and Codex CLI. aim/Codex is a separately labelled subscription cohort because its model and
upstream differ. aim/OpenRouter sends Chat Completions; Codex CLI sends Responses, so the
matched-model pair still has a documented protocol difference. The manifest limits paid OpenRouter usage to $1.50 of *provider-reported*
cost and at most 12 Codex subscription runs; concurrency is one. A missing OpenRouter cost
on a successful response stops further paid runs; an unpriced transport attempt reserves $0.05
against the cap and remains marked as missing usage. Codex auth is borrowed read-only from the existing Codex home and
checked by hash before/after each aim/Codex run. Never run `codex login` for this benchmark.

The task driver resolves pinned executable paths before replacing `HOME`, so peers cannot
silently use workstation-global `latest` shims. Each run gets a fresh HOME and workspace.
The build includes `aim`, `aimx`, and `aim-coderun`, and `AIM_CODERUN` is set explicitly for
both aim providers; the tool set cannot depend on whether another cargo task built the worker.
One scripted mock prewarm per harness is retained separately in the result; timed trials use
fresh HOMEs but warmed binary/OS caches.
Temporary harness output is discarded. The recorder persists only request/response sizes,
timings, request hashes, prefix lengths, and provider numeric usage. It does not persist raw
body text or authentication headers.

## Reading the results

`results/w22-wire.json` and `results/w22-live.json` contain per-run rows, binary SHA-256 hashes,
manifest hash, hardware, and cohort summaries. `first_request_tokens_estimate` is explicitly
`ceil(uncompressed JSON bytes / 4)`; it is a rough sizing proxy, **not** billed tokens. The live
`usage` fields come from the provider's SSE response at the recording proxy, not the harness.
ITE is predeclared as uncached input + 0.1 × cached input + 1.25 × cache writes + 5 × output.
ITE and cost per passed task include failed runs in the numerator. Subscription calls have no
reported USD cost and remain `null` in the cost comparison. `first_token_ms` measures the first
text or tool-argument delta at the proxy; startup is process launch to the first model POST.

These are paired harness measurements, not model-quality rankings. A headline win requires a
non-inferior pass rate on the same model/upstream before comparing ITE per passed task. The
six-task live suite is small: inspect task-level flips and uncertainty before drawing broader
conclusions. W5–W8 and the public suites remain follow-up work; `TIER3.md` describes the latter.
