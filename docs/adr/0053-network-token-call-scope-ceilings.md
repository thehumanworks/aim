# ADR 0053: Bind network bearer scopes to harness sessions

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0027, 0046, 0047
- Scope: Owner-issued `aimx` network bearer credentials and their harness session authority; local and SSH principals keep their existing authority.

## Context

ADR 0047 introduced owner-issued network bearer tokens with `read` and `write` authority. It deferred expressing them as W20 `CallScope` ceilings. ADR 0046 now carries a session ceiling into the verified `aim-kernel::policy::effective` fold and enforces it against backend-resolved paths. A coarse write token cannot delegate only one subtree, forbid writes beneath a prefix, or omit process execution. The policy architecture requires network clients to receive scoped grants (`docs/architecture.md`, §12).

## Decision

`aimx token create` retains `--scope read|write`. It also accepts a custom `--root` (repeatable absolute normalized prefix), `--ops` (comma-separated `read`, `write`, `exec`), and `--deny-write` (repeatable absolute normalized prefix). A custom token requires roots and operations; the legacy `--scope` cannot be mixed with custom fields. The token registry stores the W20 `CallScope` beside the token digest and expiry. Old records without the new field deserialize with no ceiling. Token values remain displayed once and are never stored.

Authentication attaches the stored `CallScope` to the network principal. A new session starts with that ceiling already bound, before any `workspace.open`; a client-supplied ceiling may only narrow it. Opening a workspace disjoint from every ceiling root is refused. Omission and resumption do not remove the token ceiling. A resumed WebSocket session and a reused HTTP session ID must match the complete authenticated principal, including its current scope, so a registry change cannot reuse older authority. The scope is evaluated by ADR 0046's existing verified policy fold for every actionable request. The existing server-configured roots and read-only setting still bound the network principal first.

Token paths must be absolute because one token can open multiple workspace roots; relative scope paths would change meaning across workspaces. Issuance and authentication reject malformed stored scopes. The server's configured principal can still refuse a token whose requested operations or roots exceed its authority.

## Consequences

Owners can issue subroot and operation-limited network tokens without changing the harness wire protocol. The registry format gains an optional `ceiling` field, preserving previously issued tokens. A custom read-only token is reported as read-only in `PrincipalInfo`. A custom token with a scope outside the server's configured authority is unusable for actions until the owner configures compatible roots; it cannot widen server authority. Expired bearer handling and transport checks from ADR 0047 are unchanged.

## Verification

`scoped_token_persists_ceiling_and_rejects_invalid_paths_or_ops` checks private persisted data, old/new decoding, and malformed scope refusal. `scoped_token_limits_network_requests_and_resume` exercises a loopback WebSocket bearer across allowed reads/writes, a disjoint workspace open, a write-denied prefix, execution denial, client narrowing, attempted widening, resumption, a registry scope change, and a legacy `read` token. `live_scoped_token_limits_network_requests_and_resume` runs the same network boundary as an ignored live smoke. The pure intersection remains established by ADR 0027's Verus policy proof; this ADR adds no kernel logic.
