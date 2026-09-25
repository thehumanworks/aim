# ADR 0008: Enforce execution grants in aimx

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0002, 0005, 0006
- Scope: Admission, principals and enforcement for execution requests.

## Context

Standalone `aimx` accepts callers other than `aim`, so the agent dispatcher's approval cannot
protect filesystem and process effects by itself (`docs/architecture.md`, §§2, 12). SSH, plugins
and programs also widen the number of sources issuing tool calls. The same pure policy decision
must govern both the admission path and the execution authority.

`docs/architecture.md`, §4.1, identifies a second failure mode: after a disconnect, retrying a
write or spawn without an outcome record could duplicate an effect.

## Decision

Use two enforcement points over one `aim-kernel` policy spec. `aim` admits dispatcher calls
under session grants ∩ agent ceiling ∩ source ceiling. `aimx` independently enforces every
request, including direct MCP and standalone clients, against its authenticated principal's
workspace roots, operation classes, protected paths and limits (`docs/architecture.md`, §§6.3,
12). A per-call grant may narrow that principal's authority but never widen it; deny overrides
allow.

Authenticate local unix-socket peers by OS peer credentials and assign configured grants.
Require scoped bearer tokens or mTLS for network callers. Bind resume and idempotency state to
the authenticated principal and harness instance (`docs/architecture.md`, §§4.4, 12).

Every mutation, including fs writes/edits/removal/renames/copies/mkdir, `exec.spawn` and a
mutating `tools.call`, carries a client-generated idempotency key. Keep a bounded per-session
dedup table: a retry within the window returns the recorded outcome; after expiry return
`unknown_outcome`, never silently execute again. Keep request acknowledgement separate from
stream sequence (`docs/architecture.md`, §4.1).

Listen on loopback or unix sockets by default. Non-loopback HTTP/WS requires authentication
and TLS or a declared protected reverse proxy. Browser-reachable endpoints check `Origin` and
CSRF-safe tokens; enforce request, body and concurrency limits (`docs/architecture.md`, §12).

## Consequences

Direct harness callers cannot bypass execution grants by avoiding the agent daemon. The same
kernel rule needs two shell integrations, with careful normalization of paths and principals.
An expired dedup entry may leave an unknown result, so clients must surface it as uncertainty
rather than retrying a possibly completed effect.

## Verification

M1a adds negative `aimx` conformance tests for unauthenticated fs/exec, cross-workspace access,
per-call widening, and protected paths. It also tests duplicate mutation keys and expired-key
`unknown_outcome` (`docs/architecture.md`, §§12, 15). The kernel policy proof is to be added in
M1a; per §13 it establishes only the pure decision given authenticated principals and normalized
paths. Symlink-race, disconnect and network listener tests check those shell assumptions.
