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
Code mode (ADR 0076) is chosen per arm: `--harnesses aim_openrouter@off,aim_openrouter@on,aim_openrouter@only`
runs aim with `AIM_CODE_MODE` set to each; an arm-less aim harness gets the caller's
`AIM_CODE_MODE` (the legacy `AIM_BENCH_CODE_MODE=off` still means `off`), else aim's default.
Arms are not in the committed wire baseline, so compare them with `--no-gate`.
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

The code-mode-off W1 diagnostic used
`AIM_BENCH_CODE_MODE=off python3 -B bench/run.py wire --cases W1 --harnesses aim_openrouter --out bench/history/code-off.json`;
since T4b a gated wire run refuses a caller's code mode, so name the arm instead:
`python3 -B bench/run.py wire --cases W1 --harnesses aim_openrouter@on --no-gate --out bench/history/code-on.json`.
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

## Wire gate repair (T4b)

`mise run bench:wire` failed on the integration branch with 35 regressions. Every one is
attributed below; none was hidden by a bound alone. The gate now compares against
`results/t4b-main-baseline.json` (named by the manifest's `[wire] baseline`), recorded at
`d211f6a` with the decided default code mode, `off`. The manifest's aim bounds changed with it, so
this is a new cohort. `results/w26-main-baseline.json` stays as W26's evidence.

**Codex CLI and pi (26 regressions): the measurement, not the peers.** Their pinned binaries hash
as in the W26 baseline. Codex, pi and aim put the workspace path in model-visible text, and pi
names its temp output file, so the caller's TMPDIR leaked into the byte counts: macOS's
`/var/folders/…/T/` is 44 bytes longer than the `/tmp` the baseline was recorded under, which
moved every codex/pi instruction size, request size and pi's W2/W3 output by exactly 44. Trial
roots and each harness's TMPDIR are now fixed at `/tmp` (`77f6233`); the codex and pi rows of the
new baseline equal W26's byte for byte. A gated run also refuses a caller's `AIM_CODE_MODE` (or
`AIM_BENCH_CODE_MODE`), which would otherwise change what the arm-less aim harness measures.

**aim (9 regressions): W1 grew from 5,844 bytes (W26's recorded candidate, code mode `on`) to
8,353 (default `off`).** At the fixed `/tmp` root:

| Change | Commit (ADR) | W1 bytes | Tools | Instruction chars |
|---|---|---|---|---|
| W26's recorded candidate | `286274a` (0056) | 5,844 | 10 | 1,440 |
| `ui_show`, `ui_update`, `ui_close`, `ui_catalog` specs | `2617afd` (0064) | +773 | +4 | |
| their names in `run_code`'s cell-tool index | `2617afd` (0064) | +42 | | |
| "Batch independent calls" prompt line | `bb1b0fa`, trimmed `2c19fc6` (T3) | +139 | | +139 |
| "# Code mode" prompt section | `b364b9c` (0076) | +241 | | +241 |
| `run_code` guidance, `Promise.all` example, typed cell-only API | `b364b9c` (0076) | +706 | | |
| **integration branch, default `on`** | `58f857e` | **7,745** | **14** | **1,820** |
| default `off`: no `run_code` (1,378) and program tools (937) | `d211f6a` (0076 §6) | −2,315 | −4 | |
| default `off`: `Glob` 434, `Grep` 1,081, `KillShell` 258, `search_sessions` 374, `read_session` 348 | `d211f6a` | +2,495 | +5 | |
| default `off`: full `Read`/`Bash`/`LS` descriptions (W26 shortens them only in code mode) | `1cdfba8` (0056) | +427 | | |
| one more tool separator | | +1 | | |
| **new baseline** | `d211f6a` | **8,353** | **15** | **1,820** |

W2's model-visible Bash output is 11,191 characters (ceiling 12,088, unchanged) and aim makes one
auxiliary request, as in W26. The manifest's `aim_expected_tools` is now 15 and
`aim_w1_max_bytes` 8,400: 47 bytes of room, so a new tool or prompt line needs an explicit
manifest change. The rest of the gate (no request growth against the baseline, append-only
prompts, identical repetitions) is unchanged.

**Real costs this keeps visible**, measured here and left for follow-ups because they are outside
this task's files or would change what the code-mode benchmark measured:

- The four `ui_*` tools (+773 bytes) reach headless `aim run` sessions, where no client can show
  a surface. ADR 0064 accepted them under an 800-byte budget; offering them only with a UI
  client attached would recover the bytes.
- The "# Code mode" prompt section (+241 bytes, `b364b9c`) is sent although the default no longer
  offers a code tool. It should follow the exposure (it is built before code mode is composed,
  in `host.rs`).
- `off` sessions do not get W26's shorter `Read`/`Bash`/`LS` descriptions (+427 bytes against
  `on`). With them, `off`'s W1 would be about 7,926 bytes.
- In `only`, a cell's nested `Bash` result bypasses ADR 0056's model view: the scripted W2 shows
  30,147 characters against 11,191 through a direct `Bash`.

Wire-tier W1 by arm on this tree (`results/t4b-wire-arms.json`, `--harnesses aim_openrouter@off,aim_openrouter@on,aim_openrouter@only --no-gate`):
`off` 8,353 bytes and 15 tools, `on` 7,745 and 14, `only` 4,998 and 4. W4's 21-request trajectory
ends at 12,675 / 12,067 / 10,760 bytes.

## Code-mode cohort (T4b)

`plans/code-mode.md` pre-registers the benchmark that sets aim's default `AIM_CODE_MODE`
(ADR 0076): arms, tasks, metrics and the decision rule, whose margins live in the manifest's
`[code_mode]`. It adds three scripting tasks to the live tier (`todo_table`, `callers`,
`doc_index`; hidden report graders in `live_tasks.py`), so this manifest starts a new cohort.
`acp_trials.py` runs `acp:claude` trials, whose model requests bypass the proxy, and
`code_mode.py` tabulates result files and applies the rule:

```sh
python3 -B bench/run.py live --harnesses aim_openrouter@off,aim_openrouter@on,aim_openrouter@only \
  --repetitions 1 --out bench/history/code-mode-r1.json
mise exec -- python3 -B bench/acp_trials.py --cases todo_table,callers,doc_index --max-runs 12 \
  --out bench/history/code-mode-acp.json
python3 -B bench/code_mode.py bench/history/code-mode-*.json
```

### Results and decision

Pre-registered at `7a788e5`; the smoke led to one grader fix (`6b1878f`: a report may sit in the
directory its prompt names, since every arm wrote `docs/INDEX.md`). Results, with no model text:
`results/t4b-code-mode-openrouter-r{1,2,3}.json` (primary), `t4b-code-mode-codex.json`,
`t4b-code-mode-acp.json` (secondary), the smoke files, and `t4b-spend-ledger.json`.
`python3 -B bench/code_mode.py bench/results/t4b-code-mode-{openrouter-r1,openrouter-r2,openrouter-r3,codex,acp}.json`
reproduces the tables and the verdict.

Scripting tasks (`todo_table`, `callers`, `doc_index`); requests are model requests per trial,
calls are per trial, ITE and USD are per passed task with failed runs in the numerator:

| Arm | Runs | Pass | Requests | Direct calls | Nested calls | ITE/passed | $/passed | p50 wall s |
|---|---|---|---|---|---|---|---|---|
| `aim_openrouter@off` | 9 | 5/9 | 6.22 | 8.89 | 0 | 13,113 | 0.0060 | 10.7 |
| `aim_openrouter@on` | 9 | 5/9 | 7.22 | 13.33 | 1.33 | 16,026 | 0.0073 | 11.1 |
| `aim_openrouter@only` | 9 | 1/9 | 12.00 | 17.22 | 33.33 | 218,307 | 0.0939 | 34.8 |
| `aim_codex@off` | 6 | 6/6 | 4.33 | 9.83 | 0 | 6,541 | — | 20.9 |
| `aim_codex@on` | 6 | 6/6 | 4.33 | 6.83 | 2.67 | 6,737 | — | 18.1 |
| `acp_claude@off` | 6 | 6/6 | 4.83 | 6.33 | — | 20,917 | — | 16.2 |
| `acp_claude@on` | 6 | 6/6 | 4.83 | 6.50 | — | 21,763 | — | 20.3 |

Existing tasks (six, OpenRouter only):

| Arm | Runs | Pass | Requests | Direct calls | Nested calls | ITE/passed | $/passed | p50 wall s |
|---|---|---|---|---|---|---|---|---|
| `aim_openrouter@off` | 18 | 18/18 | 6.83 | 6.17 | 0 | 5,770 | 0.0027 | 12.9 |
| `aim_openrouter@on` | 18 | 18/18 | 6.89 | 5.89 | 0 | 5,091 | 0.0024 | 16.3 |
| `aim_openrouter@only` | 18 | 14/18 | 13.39 | 13.67 | 10.44 | 21,982 | 0.0102 | 27.1 |

All nine OpenRouter tasks: `off` 23/27 passes, 7,367 ITE per passed task; `on` 23/27, 7,468;
`only` 15/27, 35,071. The Wilson 95% interval for 23/27 is [0.68, 0.94]; these are small samples.

**Verdict of the rule: `off`.** `on` passes (a) and (c) (12% *less* ITE on the existing tasks)
but fails (b): 16% more requests and 22% more ITE on the scripting tasks. `only` fails all three.
The secondary cohorts do not meet the per-provider bar: codex and Claude took the same number of
requests in `on` as in `off`, with 3–4% more ITE. aim's `DEFAULT_MODE` is now `Off` (ADR 0076
§6).

What the runs show beyond the rule:

- gpt-4.1-mini called `run_code` in 1 of 27 `on` runs; codex called `exec` in 4 of 6, Claude
  `run_code` in 2 of 6. Where a model scripted, it did not save requests on these tasks.
- In `only`, 97 of 292 `run_code` calls failed (54 `ReferenceError`s, mostly `require`), 109
  calls went to the program tools against an empty store, and 5 runs hit the 24-request cap.
- `on`'s lower existing-task ITE is its smaller prefix, not code: 7,745 bytes against `off`'s
  8,353 in W1, from ADR 0056's compact direct set and shorter `Read`/`Bash`/`LS` descriptions,
  which apply only in code-mode sessions. Giving `off` the shorter descriptions is a follow-up.
- `doc_index` failed 9/9 on OpenRouter in every arm (the model takes `# Options` over the earlier
  `## Command line usage`) and passed 8/8 on codex and Claude: it separates models, not arms.
- In `only`, a cell that prints a nested `Bash` result shows the model 30,147 characters of the
  W2 output where a direct `Bash` shows 11,191: ADR 0056's model view does not apply to nested
  results (wire tier, `--harnesses aim_openrouter@only`).

Spend: $0.5275 of the $3 OpenRouter budget across six invocations (three smoke, three primary;
provider-reported, no transport errors), 12 of 12 codex subscription runs, 13 of 16 Claude
subscription runs. `~/.codex/auth.json` hashed `7315f3c9…91b0` before and after the codex runs.
