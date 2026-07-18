# Phase 0 Architecture

## Purpose

Phase 0 proves the riskiest boundaries before product implementation. It is a vertical spike, not a second runtime: Pi remains the sole LLM/provider runtime and `noshd` remains the local authority.

The exit proof is an external iPhone browser observing a live Pi child and detached WSL2 mock job, then issuing one validated semantic status request while both are running.

## Decided boundaries

| Boundary | Decision |
|---|---|
| Control plane | `noshd` is a Node/TypeScript, per-user Windows daemon. GUI closure and Pi-turn completion must not stop it. One instance is protected by a named mutex or locked state file. |
| Agent runtime | Embed Pi through its supported SDK and load Pi package resources through Pi's resource loader. NOSH never calls model providers directly or keeps provider credentials. |
| Local access | The daemon API is loopback-only. Local clients acquire short-lived tokens through a current-user protected bootstrap channel; it does not listen on LAN/public interfaces. |
| Job execution | `noshd` owns detached WSL2 process groups. The spike must identify a process independently of the initiating Pi turn, stream bounded output, and recover or mark it interrupted after daemon restart. |
| Persistence | Each Project has a SQLite operational database outside Git (`%LOCALAPPDATA%/NOSH/projects/<project-id>/nosh.sqlite` or equivalent). SQLite uses WAL and foreign keys; binaries stay in an artifact store. |
| Durable truth | A durable mutation appends a persistent event and updates its projection in one SQLite transaction. Persistent events receive one monotonically increasing sequence per Project; clients recover from a snapshot plus later events. |
| Source and artifacts | Git holds code and canonical, reviewable projections; SQLite holds live operational state; artifacts are content-addressed and external to SQLite/Git; Pi retains native session trees. Runtime databases, temporary worktrees, secrets, and large artifacts are excluded from Git. |
| Wire boundary | All process boundaries use versioned `@nosh/wire` schemas. Persisted control records are strict JSON/JSONL records, never prose. IDs/timestamps/sequences are daemon-issued; JSON is RFC 8785 canonicalized before hashing/signing. |
| Remote topology | Browser and daemon both connect outbound over WebSockets through a user-owned Cloudflare relay. GitHub Pages serves static PWA files only. The relay is opaque transport/cache, never controller or plaintext processor. |
| Remote command | Commands are semantic, scoped, expiring, idempotent, and optimistic-concurrency checked by `noshd`; no arbitrary remote shell exists. Phase 0 proves only a status request. |

## Event and recovery contract

- Persistent lifecycle, approval, error, graph, review, artifact, checkpoint, and terminal-job events are retained and never dropped by coalescing.
- Token deltas, log tails, metrics, GPU samples, and heartbeats are ephemeral: they may be bounded, coalesced, or expire without changing durable state.
- Every event carries its event ID, Project ID, sequence when persistent, source, timestamp, correlation ID, causation ID, and applicable Mission/Direction/agent/job/run IDs.
- Restart reconciliation verifies the latest snapshot, active process fingerprints, and Pi cursors; it reattaches valid work and records interrupted work rather than assuming it completed.
- Filesystem/Git actions that later phases add use a durable intent: record intent, perform the external action, verify it, commit event/projection, then mark the intent complete.

## Phase 0 implementation limits

The spike may use minimal mock panels, mock jobs, and a single status command. It must not invent Mission scheduling, graph authority, provider clients, a remote shell, or a second agent transcript format. The canonical schemas, full event store, service installer, encryption protocol, and production PWA are Phase 1+ work, while retaining the boundaries above.

## Compatibility baseline

- NOSH development and release builds target Node 22 LTS (`>=22.19.0`). The maintained Pi coding-agent package declares this same Node floor, which also avoids an experimental Node runtime API in NOSH.
- The Phase 0 spike pins `@earendil-works/pi-coding-agent` 0.80.10. Any update requires the same embedding and observable-event contract tests before it can be adopted.
- The operational database uses `better-sqlite3` rather than Node's built-in SQLite module because the latter is currently experimental. This keeps WAL, transaction, and recovery behavior stable on the supported host runtime.

## Validation decisions still required before implementation proceeds

- Select the Windows per-user service mechanism and WSL2 process identity/recovery method after proving clean detach, cancellation, and restart behavior.
- Select the local bootstrap-token transport after confirming current-user ACL behavior on the supported Windows configuration.
- Finalize the crypto key hierarchy, encrypted-envelope format, rotation/revocation handling, and relay retention limits only after dedicated security review. The Phase 0 topology proof is not a cryptographic release approval.
- Pin PWA build/release provenance (lockfile, reproducible build, published hashes, strict CSP, and no third-party runtime scripts) before any remote release.
