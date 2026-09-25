# ADR 0035: Index persistent conversation events as redacted chunks

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0008, 0022, 0032
- Scope: local conversation-search projection, its persisted schema, privacy boundary and read-only tools; not automatic retrieval policy or foreign search services.

## Context

The append-only log keeps the complete transcript even after compaction (ADR 0032). Search needs a smaller, redacted projection: user and final assistant text, tool-call digests without raw outputs, and compaction summaries (architecture §6.9; `docs/research/infra.md`, §4 C1–C5). A private/ephemeral session uses `MemoryStore` and must not reach an index or Jev (architecture §5.4).

The research measured FTS5 BM25 near 0.7 ms and `sqlite-vec` at about 21–24 ms for 100,000 256-dimensional vectors (`docs/research/infra.md`, §4). The pinned `potion-retrieval-32M` model has 512 dimensions and yielded better retrieval than `potion-base-8M` in its sanity checks. The [sqlite-vec Rust example](https://github.com/asg017/sqlite-vec/blob/main/examples/simple-rust/demo.rs) initializes its SQLite extension through `unsafe` FFI. aim forbids unsafe in its own code, so this slice uses an exact in-process vector scan over the same SQLite-persisted vector blobs. It avoids an extension ABI/loading boundary with bundled rusqlite; the 100,000-chunk benchmark must decide whether this remains adequate.

## Decision

- Bump the session database schema to 2. Keep `search_chunks`, FTS5, vector blobs, index high-water marks and a durable `search_pending` queue in the existing private SQLite database. The lossless `sessions` and `events` tables remain authoritative. FTS5 indexes redacted text; one cached model2vec `potion-retrieval-32M` instance encodes vectors. The exact model revision, hashes and MIT model-card check are pinned in `search/embedding.rs`.
- Schema-1 migration queues existing event-bearing sessions so the first ordinary search can find older conversations. `--reindex` remains the explicit full rebuild path.
- Appends upsert the session's pending maximum sequence in the same transaction as events, then notify a separate indexing thread. The thread computes chunks and embeddings before a short write transaction. It deletes a pending row only when its high-water mark is no newer than the indexed snapshot. A process that exits before projection work finishes leaves the durable row for the next opener or explicit `--reindex`.
- Store only user text, the completed turn's final assistant text, a short tool-name/argument digest, and compaction summaries. Never index tool results or provider-native payloads. Redact common credentials and personal identifiers before either FTS or embedding. The index is a best-effort redacted projection under the same 0600 database permissions; the source log remains the complete record.
- Search takes FTS5 BM25 top 50 and exact cosine top 50, then fuses one-based ranks with reciprocal-rank fusion `1/(60+rank)`. Return at most three chunks per session. The optional Jev reranker receives at most 30 bounded, freshly redacted excerpts in one request, only for a persistent current session with `TYPESAFE_API_KEY`; failure retains local ranking. Automatic surfacing is a later policy integration.
- `search_sessions` and `read_session` are local, read-only agent tools over the current user's persistent database. The latter returns a bounded slice of the source log. `aim search-sessions --reindex` rebuilds the projection without changing events.

## Consequences

The FTS projection is usable before the 129 MB model download; the first explicit search verifies/downloads the pinned model and backfills vectors. Later appends get vectors in the background. A warm daemon holds about 205 MB of 512-dimensional float vectors per 100,000 chunks, plus metadata. The exact scan is linear and must be revisited if the measured p50 budget or memory target is missed. Pattern redaction cannot prove that arbitrary user-supplied secrets are absent; the private database permissions and refusal to index raw tool output remain necessary.

## Verification

- `search::chunk` tests user, final assistant, digest and compaction-summary projections, overlap, redaction and raw-output exclusion.
- `search::tests` and `store::sqlite::tests` cover FTS/vector ranking, incremental append high-water marks, workspace filtering and `MemoryStore` exclusion.
- `live_search_real_sessions_reindex_and_optional_jev` creates real OpenRouter sessions in a temporary private `AIM_HOME`, rebuilds the index, queries both tools and optionally reranks through Jev.
- The ignored 100,000-chunk synthetic benchmark in that live test reports warm p50/p95 for FTS plus the exact 512-dimensional scan. Its measurements and limits are recorded in the W16 report.
