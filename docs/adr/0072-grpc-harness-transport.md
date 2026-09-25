# ADR 0072: Carry the harness protocol over bounded gRPC streams

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0006, 0047, 0052, 0053
- Scope: The native aim to aimx network transport over gRPC. Harness methods, wire parameters, grants, and the daemon protocol do not change.

## Context

The founding brief requires aimx to run remotely over HTTP, WebSocket, **and gRPC** (`docs/vision.md`, Requirements). ADR 0006 chose `aim-harness/1` JSON-RPC for the execution protocol; ADR 0047 already carries those messages over bounded WebSocket frames and HTTP requests/SSE. A second per-method protobuf schema would duplicate the existing typed `aim-proto` definitions, require two versioning and compatibility paths, and risk different authority checks between transports. The transport only needs a full-duplex, ordered frame stream.

Tonic's [bidirectional streaming service](https://docs.rs/tonic/0.14.6/tonic/server/trait.StreamingService.html) and [HTTP/2 transport builder](https://docs.rs/tonic/0.14.6/tonic/transport/struct.Server.html) support this shape, including message, concurrency and keepalive limits. Its generated service stubs normally need protobuf code generation. This service has one RPC and one message, so hand-written prost types and tonic service glue avoid a `protoc` dependency in normal builds.

## Decision

- `aim.harness.v1.Harness/Session` is one bidirectional-streaming RPC. Its sole protobuf `Frame` has `bytes payload = 1`. Every payload is exactly one compact UTF-8 `aim-harness/1` JSON-RPC message without an NDJSON delimiter. Both adapters feed the existing `aim-rpc::Peer` via a bounded bridge. Request correlation, cancellation, notifications, method dispatch, idempotency, resume, and tool scopes therefore keep their existing semantics. Typed per-method protos and gRPC reflection are outside this decision.
- `aimx serve --grpc ADDR` shares the WebSocket/HTTP listener's bearer registry, configured root grants, token `CallScope` ceilings, TLS materials and `--behind-proxy` assertion. The bearer is required in gRPC `authorization` metadata **and** the `initialize` proof; the server binds their authenticated principal so a different credential cannot resume that session. Missing, wrong, expired, or changed-scope credentials fail without exposing registry or backend details.
- Cleartext gRPC is accepted only for a numeric loopback endpoint. A non-loopback listener requires direct TLS or an explicit protected reverse proxy declaration, as in ADR 0047. Direct TLS installs the aws-lc-rs rustls crypto provider before tonic builds its TLS configuration; linking multiple rustls providers must not make `serve --grpc` panic. Client URLs use `grpc://` on loopback and `grpcs://` elsewhere; credentials are read at connection time from `AIM_REMOTE_TOKEN` or a private `AIM_REMOTE_TOKEN_FILE`, never from the URL or session metadata. `AIM_REMOTE_CA_CERT` remains the optional local trust anchor.
- The server limits decoded and encoded protobuf messages before allocation, concurrent connections and streams, HTTP/2 keepalive, and idle sessions. It applies the existing `ServerConfig` message and in-flight limits to the bridged peer. Refused transport requests return bounded generic gRPC status, not internal error text. On a dropped stream, resume and `exec.read {after_seq}` provide the same recovery as WebSocket.
- The only schema is the tiny transport envelope, implemented with hand-written prost and tonic code. No `protoc` pin or build script is needed.

## Consequences

Every harness method becomes available over a third transport without a parallel authorization implementation. The cost is tonic/prost and their HTTP/2 dependency graph in the aim and aimx binaries, plus a small in-memory bridge per stream. Clients that speak only protobuf method messages cannot call individual harness methods; they must put JSON-RPC in the `Frame` envelope. gRPC is native-client transport here, not gRPC-Web; browser Origin handling from ADR 0047 continues to apply to browser-facing WS/HTTP.

## Verification

`aim-rpc` framing tests and the existing harness conformance calls over gRPC check opaque frame mapping, ordering, cancellation, bounds, method parity and resume. Server and client negative tests cover missing/wrong metadata bearer, a mismatched `initialize` proof, public cleartext refusal, oversized messages, scope ceilings, and wrong-principal resume. An ignored live OpenRouter test makes one edit through loopback TLS gRPC; small `fs.read` round trips compare gRPC, WebSocket and unix on the same machine. The W34 report records measured binary-size and gate-time deltas, exact check results and any remaining limits. No new pure policy function or kernel proof is introduced: admission and token ceiling decisions reuse ADRs 0027 and 0053.
