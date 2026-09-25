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
matched-model pair still has a documented protocol difference. The manifest limits paid OpenRouter usage to $1.00 of *provider-reported*
cost and at most 12 Codex subscription runs; concurrency is one. A missing OpenRouter cost
on a successful response stops further paid runs; an unpriced transport attempt reserves $0.05
against the cap and remains marked as missing usage. Codex auth is borrowed read-only from the existing Codex home and
checked by hash before/after each aim/Codex run. Never run `codex login` for this benchmark.
The proxy also reserves $0.60 before each OpenRouter model request and stops within a run when
the remaining cap cannot cover another request. The cap is per invocation; report cumulative
spend across any repeated or diagnostic invocations separately.

The task driver resolves pinned executable paths before replacing `HOME`, so peers cannot
silently use workstation-global `latest` shims. Each run gets a fresh HOME and workspace.
The build includes `aim`, `aimx`, and `aim-coderun`, and `AIM_CODERUN` is set explicitly for
both aim providers; the tool set cannot depend on whether another cargo task built the worker.
One scripted mock prewarm per harness is retained separately in the result; timed trials use
fresh HOMEs but warmed binary/OS caches.
Temporary harness output is discarded. The recorder persists only request/response sizes,
timings, request hashes, prefix lengths, and provider numeric usage. It does not persist raw
body text or authentication headers.
The recorder keeps usage if the harness closes after receiving a completed response, and marks
that client disconnect separately from the upstream HTTP status. `request_usage` retains the
numeric cached/input tokens of each live request. The grader copies only the module under test
into a fresh temporary HOME and runs Python with `-I`; it receives no provider credentials or
workspace import hooks. A grader timeout fails that trial.

## Reading the results

`results/w22-wire.json` and `results/w22-live.json` contain per-run rows, binary SHA-256 hashes,
manifest hash, hardware, and cohort summaries. `first_request_tokens_estimate` is explicitly
`ceil(uncompressed JSON bytes / 4)`; it is a rough sizing proxy, **not** billed tokens. The live
`usage` fields come from the provider's SSE response at the recording proxy, not the harness.
ITE is predeclared as uncached input + 0.1 × cached input + 1.25 × cache writes + 5 × output.
ITE and cost per passed task include failed runs in the numerator. Subscription calls have no
reported USD cost and remain `null` in the cost comparison. `first_token_ms` measures the first
text or tool-argument delta at the proxy; startup is process launch to the first model POST.
`peak_rss_mb` samples the harness process group and includes the aimx helper; the harness-only
peak and sampled helpers are also recorded separately. The executable paths come from Cargo
metadata, including `CARGO_TARGET_DIR` overrides.

These are paired harness measurements, not model-quality rankings. A headline win requires a
non-inferior pass rate on the same model/upstream before comparing ITE per passed task. The
six-task live suite is small: inspect task-level flips and uncertainty before drawing broader
conclusions. W5–W8 and the public suites remain follow-up work; `TIER3.md` describes the latter.

## W26 token and startup cohort

`results/w26-before-wire.json` and `results/w26-before-live.json` preserve the current-main
baseline. `results/w26-after-wire.json`, `results/w26-final-live-openrouter.json` and
`results/w26-final-live-codex.json` record the integrated branch. W26 reruns the three
mise-pinned harnesses; W22's optional peer measurements remain in `w22-wire.json`. Every
timed run uses a fresh isolated HOME, so these are cold-HOME startup measurements.

`results/w26-main-baseline.json` remeasures current main `263efb6` with the W26 recorder.
The W4 headline is `append_only_steps` and the provider-rendered `stable_head_ratio_median`:
tools and system content are rendered before the growing conversation. The raw JSON prefix
length remains a diagnostic of request serialization only. `bench:wire` compares against the
committed baseline and exits nonzero on a failed trajectory or deterministic regression; it
reports latency without gating it. The evaluator and its baseline must be run from a protected
copy when used by the self-improvement gate (ADR 0020).

The code-mode-off W1 diagnostic uses
`AIM_BENCH_CODE_MODE=off python3 -B bench/run.py wire --cases W1 --harnesses aim_openrouter --out bench/history/code-off.json`.
`python3 -B bench/output_budget.py --out bench/history/output-budget.json` alternates the
15,000- and 5,500-byte per-end Bash model-view budgets on the same graded live task.
`results/w26-codex-cache.json` records numeric provider usage from the ignored
`live_codex_cache_ten_steps` smoke test. Live `request_usage` contains only numeric
provider usage per request; no request body or credential is retained.
`results/w26-openrouter-cache.json` pairs ten serial live OpenRouter calls on the task-start
main source and the W26 branch with the same stable instruction/session prefix. It records
provider input, cached and cost usage per request. OpenRouter's profile uses `cache_control`
and `x-session-id`; it does not send `prompt_cache_key` because that field is not configured
for this endpoint. The main-source probe test was injected into a temporary source archive,
then removed after measurement.
The final rebased OpenRouter pair uses one repetition of all six tasks; the earlier two-repetition
candidate and focused diagnostics consumed part of W26's cumulative $1 cap. The final Codex
subscription artifact covers four tasks after an earlier six-task candidate, the ten-step cache
probe, and the media-search smoke used the 12-run cap. `results/w26-codex-catalog-timing.json`
records a read-only live catalog GET and ETag revalidation without a generation request;
`results/w26-codex-startup-mock.json` separates warmed local startup from that network GET.
`results/w26-spend-ledger.json` totals every W26 paid invocation, including superseded
diagnostics and the two unpriced transport reserves. The one direct Sonnet 5 smoke has a
conservative price bound from its numeric token usage and the published model rates.
