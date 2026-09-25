# ADR 0007: Persist sessions as typed append-only events

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0002, 0006
- Scope: Session durability and privacy boundary; provider retention remains external.

## Context

The brief requires daemon-backed persistent sessions by default and ephemeral/private modes
(`docs/vision.md`, Ideas and goals). Context compaction, forks and replay need the original
provider content, including opaque reasoning items, rather than just a rendered transcript.
`docs/architecture.md`, §5, defines a lossless raw log and derived views.

The live probe reports 30-day retention for codex dictation audio
(`docs/research/live-probes.md`, ChatGPT codex backend). A local memory store cannot promise
that a third-party provider or service retains nothing.

## Decision

Append typed `SessionEvent`s with `{schema, session_id, seq, turn_id, ts}` to one session log.
Include messages, provider-native opaque items, tool calls/results, steering, compaction, usage,
errors and UI/plugin events. Keep the raw durable transcript lossless; derive the model context,
search index, hooks, audit and telemetry as projections (`docs/architecture.md`, §5.1).

Store logs and related state in `~/.aim/aim.db` using SQLite WAL behind one DB actor thread.
Store large outputs, artifacts and media as content-addressed blobs under `~/.aim/blobs/`, with
event references. A fork is `{parent, fork_seq}` and shares its parent's immutable prefix and
blobs (`docs/architecture.md`, §§4.4, 5.2–5.3).

Protect the 0600 raw store, which may contain secrets supplied by a user or tool. Redact
credential values from all derived projections. Backup the DB and blob directory together.
Version stored events independently from wire protocol generations (`docs/architecture.md`,
§§4.4, 5.3).

For CLI/TUI `--ephemeral` and web private sessions, construct a `MemoryStore` and do not touch
daemon storage, history or indexes. Indexers, memory writers and telemetry require a
`Persistent` session witness unavailable to an ephemeral session. Private/ephemeral content
never goes to Jev, embeddings or evolution telemetry (`docs/architecture.md`, §5.4).

Request `store:false` from codex/OpenAI-compatible providers where defined. Start Claude ACP
sessions with `persistSession:false`; refuse private mode if the pinned adapter cannot guarantee
no local transcript. Disclose codex dictation's 30-day server retention before use
(`docs/architecture.md`, §5.4; `docs/research/live-probes.md`, ChatGPT codex backend).

## Consequences

Replay and compaction retain exact provider sidecars and forks avoid copying prefixes. The DB
actor serializes writes, while storage migrations and blob GC require explicit care. Local
privacy is enforceable by types; external retention must be stated per backend.

## Verification

M2a adds crash/replay and fork-prefix tests at the SQLite actor, plus a `MemoryStore` test that
leaves DB, blobs, history and indexes untouched (`docs/architecture.md`, §§5, 15). M2b's Claude
`live_private_session_no_transcript` smoke test must check the adapter leaves no transcript on
disk; provider retention disclosures need UI verification when their service lands.
