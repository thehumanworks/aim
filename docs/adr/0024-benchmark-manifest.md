# ADR 0024: Make benchmark wins depend on a fixed manifest

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0013, 0022
- Scope: Comparative performance and token-efficiency claims; not provider quality claims.

## Context

A zero-latency mock pilot measured first-request static prefixes of about 9.7k tokens for
codex, 1.5k for pi, 7.1k for oh-my-pi, and 0.7k for unreal-agent. Warm median time to first
request was 142/228/895/51 ms respectively; peak RSS was 105/147/434/16 MB. These are one
machine, prompt, model, and mock, not a task-quality ranking (`docs/research/landscape.md`,
§F1, lines 60–98; `docs/architecture.md`, §14, lines 687–695).

## Decision

Define every “beat” claim through a committed benchmark manifest: identical tasks,
model/provider/effort, tool permissions, hardware, local or remote topology, cache state,
and repeated trials for each harness. The headline metric is input-token equivalents (ITE),
or cost, per passed task at a non-inferior pass rate on the same model, effort, and upstream.
Grade task quality independently before comparing token, latency, and cost results. Read
provider usage through a recording proxy, separating input, cached, cache-write, output,
and reasoning tokens; do not trust a harness's own summary
(`docs/architecture.md`, §14, lines 697–704;
`docs/research/landscape.md`, §I1, lines 561–567, 599–620).

Run three tiers: offline wire cases on every PR (fixed prompt size, prefix stability, output
bounds, latency, memory); a nightly live port of tny `harness_bench`; per-release Harbor runs
on Terminal-Bench 4.0, SWE-Atlas QnA, DeepSWE 1.1, and a SWE-bench Verified subset. Record
binary hashes and per-task results, rotate harness order, and bound concurrency. Benchmark
WebSocket incremental input separately from common HTTP/SSE token comparisons
(`docs/research/landscape.md`, §I1, lines 569–620).

M10 passes only on a measured win in predeclared dimensions without material quality loss.
If a peer still wins, report the exact workload and metric. Levers to measure include fewer
turns, compact output handles, stable prefix and cache routing, lazy tool loading, compaction,
Jev routing, and startup without provider I/O (`docs/architecture.md`, §14, lines 689–704).

## Consequences

The manifest and peer adapters require maintenance. A single faster mock request or a smaller
prompt cannot establish a better harness; quality and matched live runs decide that claim.

## Verification

In M10, require `bench/wire` W1–W8 on every PR, a nightly live run with proxy-read usage and
paired quality intervals, and per-release Harbor artifacts. Publish the manifest, hashes,
task-level outcomes, confidence bounds, and every gap alongside any “beat” statement.
