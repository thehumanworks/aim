# ADR 0013: Batch Jev advice behind verified decision boundaries

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0007
- Scope: Jev-supported effort, routing and relevance; Jev is never an authority source.

## Context

A live Jev score plus Noul probe took 0.60 seconds for one request, making per-step use
plausible when overlapped with tool execution (research/live-probes.md, TypeSafe Jev, lines
24–29). Jev questions share one request and the reported input price is $0.042/M tokens
(research/infra.md, TL;DR, lines 55–59). tny ADR 0165 exposed Jev decisions through an
explicit CLI; it did not establish an automatic policy (research/tny-lessons.md, §Jev,
lines 220–262). The pure controller is a suitable Verus target, but floats are not
reliably verified in the present toolchain (research/verus.md, §F5, lines 421–450).

## Decision

- Keep `aim-jev` as an aim module first, behind `Decider`, with a `typesafe-jev`
  implementation on `spawn_blocking` and a deterministic fallback when no key, timeout or
  valid answer is available. Never block tool completion waiting for Jev.
- Issue one bounded, deadline-bound bundle while tools execute. Ask effort Score over the
  selected model's catalog ladder; Noul for stuck, progress and need for past sessions;
  and relevance of candidate skills. Apply the result to the next provider request only.
- Route models by deterministic capability filtering first (context, tools, images, tier),
  then Jev Choice among eligible configured models. Keep a sticky route per task and account
  for prompt-cache loss before switching. A probability never grants a capability.
- Use Jev relevance for compaction elision, memory recall, skill/program suggestions and
  search reranking, with the original lossless transcript preserved.
- Validate and clamp Jev probabilities, then quantize to fixed-point basis points at the
  kernel boundary. The verified integer effort controller enforces catalog and user/agent
  bounds, at most one ladder step per decision, hysteresis against flip-flopping, and an
  explicit user override. Log each input and output for offline threshold evaluation.
- Do not send private or ephemeral session content to Jev.

## Consequences

Jev can improve decisions without becoming a permission source or adding a model turn
to the critical path. Its quality and cost still require empirical measurement; the
proof establishes controller bounds, not that Jev chose a good model or effort.

## Verification

- M6 kernel spec `effort::next` and theorems `effort::bounded`,
  `effort::one_step` and `effort::hysteresis` are to be added over integer inputs.
- M6 live test `live_jev_batched_bundle` records request count, latency and output shape;
  fallback and private-session tests verify no Jev request is sent in those cases.
