# ADR 0056: Reuse startup capabilities and bound model-visible shell output

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0018, 0024, 0025, 0032
- Scope: Native-session startup, code-mode prompt shape, the model-visible Bash result, and the W26 benchmark gate. The aimx output ring and handle retention do not change.

## Context

W22 measured a 9,601-byte first request with code mode, 30,146 model-visible characters after a 589 KB shell result, and 437.59 ms median live startup on OpenRouter (`scratchpad/reports/W22-bench.md`). The W26 baseline on current main measured 10,529 bytes and the same 30,146 characters (`bench/history/w26-before-wire.json`). Startup traced to repeated serial catalog reads for skill sizing, code-mode selection, effort and compaction, plus eager media search-model discovery. Code mode also repeated a prose tool index even though the underlying tools and `describe(name)` expose their schemas.

REV15 reproduced that the W4 raw JSON prefix ranking reflects top-level key order: in provider render order, aim, Codex CLI and pi all kept the prior prompt append-only across 20 steps (`scratchpad/reviews/REV15-bench.md`, H1). It also reproduced discarded usage after a client disconnect, a wire gate that could not fail, grader import/environment leaks, and aimx RSS omitted from aim's total (H2, H3, M1, M4). The pinned current-main remeasurement is `bench/results/w26-main-baseline.json`.

## Decision

Codex and other catalog-dependent backends fetch one bounded capability snapshot concurrently with resource discovery and reuse it for the skill budget, effort ladder, code mode and compaction window. OpenRouter and AI Gateway start with portable `run_code` and the unknown-window skill budget; their bounded catalog lookup runs in the background after the first turn begins. Their provider instance is shared across sessions with one in-flight catalog fetch and a five-minute in-memory TTL (one-second retry after failure). A conservative 8,192-token fallback may only establish that the estimated context is **below** its compaction threshold. If the estimate reaches that threshold, or compaction is forced, wait for the catalog result before deciding. If lookup fails, keep the prior missing-window behavior: no proactive compaction, and on a provider-reported overflow use the current estimate as the forced window. Resolve the Codex media search model when search is called; an ephemeral session does not build credential-based media tools.

List only nested tool names in the code-mode prompt and direct the model to `ALL_TOOLS`, `describe(name)` and `search(query)` for details. Keep `Read`, `Write`, `Edit`, `LS`, `Bash`, `BashOutput` and composed service tools directly visible; less common built-in file and session-search tools remain callable inside the cell. The runtime still receives their full specs and nested calls retain the same dispatcher and authority. For a Bash result with a retained output handle, show at most 5,500 bytes from each end, separated by a recovery note naming the handle, the read call, and exactly how many preview bytes this layer omitted. Preserve the handle, exit status and the aimx output stream for later reads. Results without a handle are not shortened by this layer.

The wire gate compares each scripted trajectory with the committed current-main baseline. Any failed trajectory, request-count change, missing recovery hint, growth of deterministic bytes or output exposure, or loss of an append-only provider-rendered prefix fails the gate. W1 and W2 also have explicit 6,000-byte and 12,088-character ceilings; ten directly advertised aim tools and at most one auxiliary request are acknowledged W26 changes. Two repetitions must agree on deterministic fields. Startup and sampled whole-process RSS are reported, not gated. The live recorder keeps numeric provider usage after client disconnects; the grader copies only the module under test into an isolated Python process with a scrubbed environment and temporary HOME. The OpenRouter cohort has a $1.00 cap with a $0.60 per-request reserve checked within the proxy.

## Consequences

The Chat and Codex request serializers place stable fields before the growing `messages` or `input` array. This improves the raw JSON-prefix diagnostic; provider-reported cached tokens remain the measure of actual cache reuse. `AIM_BASH_MODEL_END_BYTES` can set 1 through 15,000 bytes per end when a session needs another output budget; the default is 5,500.

A provider catalog change is reflected when a new session starts, or when the model changes. An unavailable catalog leaves proactive compaction unavailable, as before. A warm-HOME disk cache is deferred: a catalog file adds a persisted format and ownership/privacy work, while the bounded background lookup removes the blocking cold path. Models may need one extra cell call to inspect a nested tool schema or reach a less common tool. Large shell output uses less context, while a model needing the omitted middle must read it by handle.

The committed runner and graders are still candidate-writable in an ordinary checkout; ADR 0020's isolated gate must execute its protected evaluator copy. The fixed $0.60 reserve is specific to this manifest's model and reviewed cost bound. A new model or higher cap needs a new price-bound review before using the paid runner.

## Verification

`bench/results/w26-before-wire.json` is the current-main baseline. `bench/results/w26-after-wire.json` and the live before/after artifacts measure request size, W2 exposure, startup, cache usage, pass rate and ITE. The agent's `shell_model_view_keeps_ends_and_recovery_handle` and `resumed_long_context_waits_for_catalog_before_compaction_decision` tests check the boundaries; `mise run check` is the integration gate. The fallback threshold and 5,500-byte policy are pure budget logic worth moving into `aim-kernel` if generalized.
