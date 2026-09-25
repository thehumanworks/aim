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

## Amendment, 2026-09-25: cohort 2 with exact-entry graders

Registered after cohort 1's results and before any cohort 2 run. Cohort 1 is the set of results
listed in `bench/README.md` ("Results and decision", `t4b-code-mode-*.json`).

**Why.** The codex review of T4b (REV-T4b B1) found that the three scripting graders accepted wrong
reports, because they matched basenames or substrings instead of whole entries: `todo_table`
accepted the right counts under `wrongdir/` paths, `callers` accepted a wrong function name on a
line naming a real call site, and `doc_index` accepted headings under wrong paths. This plan's
last rule bars grader changes after the smoke, so the fix starts a new cohort instead of
amending cohort 1.

**The fix.** Each hidden test now reads its report as exact entries and requires every entry to be
right, with none missing, extra or listed twice:

- `todo_table`: the table's rows by column (`TODO`, `FIXME` from the header); each file named as
  `src/NAME` (or `NAME`, relative to the `src/` the prompt names); a row that names no file, such
  as a total, is ignored.
- `callers`: each line with a `path:line` must be exactly `path:line function_name` with the path
  relative to the repository root, as the prompt says; the method may be named `Worker.setup`.
  The set of entries must equal the five calls.
- `doc_index`: each bullet must be `path: heading`, the path relative to the root or to `docs/`,
  and the heading equal to the first heading (ignoring case and Markdown decoration).

Markdown decoration (bullets, backticks, bold, links, table pipes) is ignored, so the fix only
rejects wrong answers. `test_scripting_graders_accept_a_right_report_and_reject_near_misses`
reproduces the three false passes and shows they now fail.

**Cohort 1 re-graded offline.** Every kept cohort 1 report (the 27 primary scripting trials and the
12 codex trials; `run.py --keep-outputs`) was re-graded with the fixed graders: no verdict
changed. `acp_claude` trials kept no reports, so their pass counts stay those of the lenient
graders; their request, call and ITE numbers do not depend on grading.

**What cohort 2 runs.** The primary protocol again, unchanged: `aim_openrouter@off|on|only`, all
nine tasks, three repetitions, one `bench/run.py live` invocation per repetition with the same
arm-order rotation, and `--keep-outputs`. No smoke. The secondary cohorts are not rerun: their
subscription caps are spent (codex 12/12, Claude 13/16 against 12 needed).

**What else differs from cohort 1.** Cohort 2 measures the current tree, which differs from cohort
1's (`6b1878f`) beyond the graders; every difference is recorded here so none is mistaken for an
effect of code mode:

- `7afcf10`: the `off` arm no longer carries the 241-byte "# Code mode" prompt section.
- The integration head's REV-T4a-b residuals (`3f4ee26`, `0c0a55c`, `ded3ab0`): code cells bound
  a script's failure and nested errors like their output, with a 25-token floor.
- `d211f6a` changed aim's default to `off`; every arm sets `AIM_CODE_MODE` explicitly, so this
  does not change what an arm runs.

**The decision** is recomputed with `bench/code_mode.py` from cohort 2's primary results alone, by
the same rule and margins. Arms, tasks, prompts, metrics and margins are unchanged; the
manifest's `[code_mode] cohort` names the cohort.
