# ADR 0029: Expose media services through the agent daemon

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0006, 0007, 0009, 0022
- Scope: Hosted Codex media services and the `media.transcribe` daemon contract; not recording audio or implementing a TUI microphone.

## Context

The vision calls for dictation, image generation and web search beyond Codex model turns (`docs/vision.md`). The Codex research identifies separate endpoints for these services (`docs/research/codex-backend.md` §9). A live transcription probe returned `asset_ttl: "30d"`: uploaded audio is retained by ChatGPT for 30 days (`docs/research/live-probes.md`). An ephemeral aim session only promises that aim does not persist its own transcript; that promise cannot erase provider retention.

UIs use `SessionClient` both in process and over `aim-daemon/1` (`docs/architecture.md` §4.2). The same dictation request therefore needs a typed method on that trait and the daemon protocol.

## Decision

`media.transcribe` accepts `{ audio: Base64Bytes, format, accept_retention?, session? }` and returns `{ text }`. `wav` is the supported format, with at most 25 MiB of decoded audio, matching the earlier implementation documented in `docs/research/codex-backend.md` §9. An optional session ID selects that session's persistence policy. An existing ephemeral or private session refuses the request unless `accept_retention` is explicitly `true`. The host checks policy before opening the provider client or sending audio. An unknown session ID fails closed. A request without a session is a standalone transcription and is not represented as a private aim session. The method documentation discloses the provider's 30-day retention.

The daemon frame limit is 36 MiB, enough for the base64 representation of the 25 MiB audio limit plus JSON framing. The initialized daemon advertises this limit to clients.

The Codex media client provides transcription, hosted web search and image generation independently of model turns. Search queries from private sessions may use hosted search because the query is the content deliberately sent by that tool; this is separate from uploading retained audio. Image output is written into the session workspace through the harness. The service tool host gains a `write_blob(path, bytes, key)` operation so binary writes use `fs.write`, including over an SSH-shadowed workspace, rather than local filesystem access.

## Consequences

UIs can call dictation through the same `SessionClient` interface in daemon and in-process modes. They must present the 30-day retention disclosure before setting `accept_retention` for a private session. The transcript response contains text only; uploaded audio and provider asset pointers are not added to the session log. Media capabilities can be unavailable independently of a provider's model-turn capability.

## Verification

`crates/aim/tests/daemon.rs::transcription_contract_checks_session_privacy_before_provider_access` checks the base64 wire representation and refusal through both the in-process host and the unix daemon client without contacting the provider. The Codex media client's live transcription smoke test exercises the hosted endpoint separately.
