# ADR 0041: Index durable session summaries

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0007, 0026, 0035
- Scope: SQLite session summary projection and schema migration.

## Context

REV7 measured `summarize(50)` at roughly 0.5 s on 1,000 sessions and 300,000 events because
the query reconstructed every visible event lineage before applying `LIMIT`. The daemon idle
tick also used that query every 200 ms (REV7-daemon-mcp.md, M4). Fork summaries must reflect
the visible parent prefix, including the final visible timestamp and maximum turn, while later
parent appends must not change a child's summary (ADR 0026).

## Decision

Schema v3 adds `session_stats(session_id PRIMARY KEY, turns, last_activity_ms)` and an activity
index ordered by descending timestamp and session id. Session creation inserts the metadata and
summary atomically. A fork starts with the exact visible parent-prefix maximum turn and final
visible event timestamp, or its creation time for an empty log. An event append updates the
summary in the same transaction, taking the maximum turn and the last appended event's timestamp
even when timestamps regress. Existing databases are backfilled once using the original fork-aware
lineage query before the schema version advances. A v1 migration also queues existing events for
conversation search (ADR 0035); v2 keeps its existing search queue. `summarize(limit)` reads the activity index and
session metadata up to the limit without scanning `events`.

## Consequences

Writes maintain one extra projection row. Existing databases pay the lineage cost once at
migration; subsequent listings are bounded by the requested count. The projection is derived
from the event log and must be updated transactionally with it.

## Verification

Store tests compare fork prefixes, later parent appends, nonmonotone turns and timestamps, and
v1 migration. An `EXPLAIN QUERY PLAN` test requires the activity index and no `events` scan or
temporary sort for the summary query.
