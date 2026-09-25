# Code-mode default: benchmark plan (T4b)

Pre-registered before any live run of this cohort. The rule's margins are data in
`bench/manifest.toml` `[code_mode]`, and `bench/code_mode.py` applies them to the recorded
results, so neither can move once results exist. Results and the decision go to
`bench/README.md` ("Code-mode cohort") and ADR 0076's evidence.

## Question

Which `AIM_CODE_MODE` should aim use when none is set: `off`, `on` or `only` (ADR 0076)? Today
the default is `aim_kernel::code_mode::DEFAULT_MODE = On`, marked provisional. The maintainer
prefers code mode, but only once a benchmark shows its effect on tool-calling efficiency. That
preference is what this cohort tests; it is not assumed.

## What earlier runs predict

- W26's final live cohort: the OpenRouter model never called `run_code`; Codex called `exec`
  once in 20 tool calls (`bench/results/w26-final-live-*.json`).
- T4a's single runs (ADR 0076, "Live evidence"): gpt-4.1-mini in `on` answered with `Bash`
  and never called `run_code`. In `only` it used `run_code` and was right in all five runs, but
  took 2–16 requests, tried `require('fs')` and once spent 26 of 30 calls on `list_programs`.
  Claude (sonnet, `acp:claude`) used one `run_code` cell in both `on` and `only`.

So for gpt-4.1-mini, `on` may behave like `off` with a smaller prefix (its compact set hides
five tools), and `only` may save requests on fan-out tasks but cost more on edit-and-test tasks.
The rule below can conclude `off`.

## Arms

**Primary** (decides the global default): `aim_openrouter@off`, `@on`, `@only` through
`bench/run.py live`. The manifest's model (`openai/gpt-4.1-mini`), one set of debug binaries,
`AIM_CODERUN` explicit, `--max-requests 24`, a 180 s timeout, fresh HOME and workspace per trial,
harness order rotated per task and repetition.

**Secondary** (descriptive; they can motivate a per-provider default, not the global one):

- `aim_codex@off` and `@on` (`gpt-6-sol`, low effort, the ChatGPT subscription): the three
  scripting tasks, 2 repetitions, 12 runs. Codex is aim's default provider, so its behavior
  matters for the default even if its cohort is small.
- `acp_claude@off` and `@on` (`claude-agent-acp` 0.81.2, `-m sonnet`, the Claude subscription):
  the three scripting tasks, 2 repetitions, 12 runs, through `bench/acp_trials.py`.

## Tasks

The six existing live tasks (`even_sum`, `clamp`, `slug`, `retry`, `ledger`, `normalize`) fix
one function and run its tests. Three new tasks reward scripting: they fan out over many files
and end in one written answer. Their tests are hidden from the workspace and grade the report.

| Task | Fixture | The model must | The hidden test checks |
|---|---|---|---|
| `todo_table` | 10 files in 5 languages under `src/`, 0–5 TODO and 0–3 FIXME each | write `REPORT.md`: a table of per-file counts | every file's two counts, files with none included |
| `callers` | 11 Python modules | write `CALLERS.md`: `path:line function` for each call of `load_config` | the five call sites; any other `path:line` fails (decoys: `load_config_file`, an import, a docstring, a comment) |
| `doc_index` | 10 Markdown files in 3 directories and a `.txt` decoy | write `INDEX.md`: each file's first heading | each heading (front matter is not one; one file starts with a paragraph, one with `##`); no `.txt` entry |

`test_scripting_graders_accept_a_right_report_and_reject_near_misses` validates each grader
before any run.

## Protocol

1. **Smoke** (excluded from the decision): one repetition of the three scripting tasks in the
   three OpenRouter arms (9 runs), and one `acp_claude@off` run of `todo_table`. It may only lead
   to fixing a grader that rejects a right answer or a driver bug, recorded with the results.
2. **Primary:** 3 repetitions of all 9 tasks in the 3 arms (81 runs), one `bench/run.py live`
   invocation per repetition: the manifest's $1.00 cap with its $0.60 per-request reserve stops
   an invocation once about $0.40 is spent.
3. **Secondary:** the codex and Claude cohorts above.

Budget: OpenRouter spend at most $3 in total (expected about $0.5), at most 12 codex and 16
Claude subscription runs. `~/.codex/auth.json`'s SHA-256 is taken before and after the codex runs.

## Metrics

Per arm, for the scripting tasks, the existing tasks and all nine:

- pass rate with its Wilson 95% interval, and per-task passes;
- mean model requests per trial (recorded at the proxy);
- mean direct tool calls (the model's own) and nested tool calls (made by a code cell, i.e. a
  `tool_started` update that names a `parent`) per trial;
- ITE per passed task (ADR 0024: uncached input + 0.1 × cached + 1.25 × cache writes + 5 × output;
  failed runs count in the numerator) and USD per passed task;
- p50 wall time.

A trial that hits the request cap or the timeout fails. For `acp_claude`, requests are the
distinct assistant message ids in Claude Code's own transcript of the trial's workspace, ITE comes
from the ACP turn usage aim reports, and nested calls are not observable (the relay emits no child
events, ADR 0076 §4). The codex cohort is recorded at the proxy like OpenRouter.

## Decision rule

`off` is the reference. A candidate (`on` or `only`) qualifies only if all three hold:

- **(a) Quality:** its pass rate over all nine tasks is at least `off`'s minus 8 points
  (`pass_noninferiority_delta_pp`).
- **(b) Scripting efficiency:** on the three scripting tasks, its mean model requests per trial
  *or* its ITE per passed task is at least 25% below `off`'s, and the other of the two is at most
  10% above `off`'s.
- **(c) No regression elsewhere:** on the six existing tasks, its ITE per passed task is at most
  10% above `off`'s.

If both qualify, the one with the lower ITE per passed task over all nine tasks becomes the
default. If neither qualifies, the default becomes `off`.

The secondary cohorts are measured the same way. A per-provider default is warranted only if a
cohort clearly contradicts the global verdict: the mode the rule rejected meets (b) there with no
fewer passes, or the chosen mode loses two or more passes against the other. It must be data (a
profile or catalog field), never a check of a provider id or model; if that is not cheap here, it
is recorded as a follow-up with the evidence.

## Changes allowed after results

- Cheap, general tweaks only: hiding the program tools from the model, and the wording of the
  code tool's description. Never wording aimed at these tasks. Each in its own commit.
- A tweak re-runs every arm it affects with the full primary protocol, and the decision uses only
  the post-tweak runs of those arms. Earlier runs stay in `bench/results/` and in the report.
- Tasks, graders, arms and margins do not change after the smoke.
