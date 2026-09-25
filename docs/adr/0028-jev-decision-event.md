# ADR 0028: Record bounded Jev decisions as typed session events

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0013
- Scope: Durable Jev effort decision evidence and session updates.
- Amended by: [0038](0038-session-authority-effort-source-and-config-outcomes.md)

## Context

ADR 0013 requires every bounded effort decision to be logged for threshold tuning. The session
log in ADR 0007 stores typed events, while the daemon streams typed updates. Jev scores and
probabilities enter as floats but the kernel accepts only quantized integers (ADR 0013;
research/verus.md, §F5).

## Decision

Add `DecisionRecord` to `aim-proto` and carry it in `EventBody::Decision` and
`SessionUpdate::Decision`. Record the model and ladder, controller inputs, raw Score values,
quantized Score and Noul values, chosen index, latency, and reported token cost. The record does
not contain the transcript or credentials. The current effort is also emitted through
`ConfigChanged` when it changes.

When automatic effort is enabled, start at the catalog's declared default effort, or its lowest
supported level when no default is declared. Send that explicit level on the first provider
request so the controller's current index describes the effort actually in force. If catalog
lookup fails, keep the provider default and disable automatic advice for that session.

## Consequences

An additive event enables offline tuning and chronological replay without putting Jev text into
the conversation. Older event readers preserve unknown kinds as `EventBody::Unknown`.

## Verification

The `live_jev_batched_bundle` smoke test covers a real typed bundle; agent tests check the
decision update is emitted only before the next model request.
