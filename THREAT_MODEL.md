# Phase 0 Threat Model

## Scope and security objective

NOSH is a single-user, local-first system. The Windows workstation and `noshd` are authoritative; the relay provides opaque delivery only. Phase 0 validates these trust boundaries. It does not claim that the final encryption design is complete or security-reviewed.

## Assets and trust boundaries

| Asset | Required boundary |
|---|---|
| Provider, Git, integration, and device private credentials | Pi configuration, OS credential store, or encrypted local vault; never Project files, SQLite, events, logs, artifacts, browser bundle, or relay payloads. |
| Research source, prompts, code, graphs, and results | Project/daemon authority; remote copies are end-to-end encrypted. The relay must not receive plaintext. |
| Command authority | `noshd` validates every command after decryption; a GUI, relay, Pi response, Git commit, or artifact cannot mutate operational state directly. |
| Project isolation | Canonical Project roots, Project IDs, independent database/encryption/branch/worktree scopes, and scoped IDs prevent cross-Project access. |
| Evidence and history | Append-only persistent events, content hashes, immutable evaluated commits, and retained signed commands support audit and recovery. |

## Threats and required controls

| Threat | Required control |
|---|---|
| Curious or compromised relay | Outbound-only daemon connection; end-to-end encrypted and signed envelopes; relay stores only ciphertext, opaque routing/sequence data, public device keys, and revocation metadata. |
| Stolen device or browser profile | Per-device signing/key-agreement keys, password-protected local vault, explicit pairing approval, individual revocation, and short-lived pairing capabilities. Password knowledge alone cannot enroll a device. |
| Replayed, stale, or forged remote command | Validate device signature, revocation state, command/idempotency IDs, expiry, Project/target scope, permission, expected version, and legal transition. Return conflict/rejection; never silently apply stale state. |
| Malicious PWA delivery or dependency | No third-party runtime scripts, strict CSP, pinned dependencies/lockfile, reproducible builds with published hashes, protected release/CI, and user confirmation before activating an update during an active Mission. |
| Prompt injection in papers, repos, datasets, logs, or web content | Treat external material as untrusted content, not instructions. Role/tool policy and daemon enforcement—not model text—grant authority. Reviewer checks provenance for high-impact conclusions. |
| Agent role escalation or unsafe tool use | Typed, scoped daemon tools; role, Project, network, branch, and lifecycle policy checks; workers cannot create workers; no arbitrary remote shell. |
| Path traversal or Project confusion | Resolve and validate canonical roots before use; derive paths from validated Project/worktree IDs; enforce Project-scoped database, branch, and artifact references. |
| Secret disclosure through observability | Redact known token formats/configured sensitive fields; exclude `.env` from Git and previews by default; apply redaction and size policy before previews, diagnostics, events, handoffs, or relay transport. |
| Daemon/GUI/network failure | Per-Project event sequence, snapshots plus replay, durable external-operation intents, process fingerprint reconciliation, idempotent commands, and read-only remote state when the daemon is unavailable. |
| Unintended Git publication or protected-branch change | Local writes only in owned scopes; protected-branch merge requires explicit approval; pushes require an explicit Mission envelope or direct Normal-mode user action; PR/release/visibility changes require separate authorization. |

## Security invariants

- No plaintext provider secret, project content, source code, graph, result, or artifact reaches the relay.
- A remote command is not executed until live `noshd` acknowledges it; offline clients may save drafts only.
- LLM prose, Markdown, and JSON code fences never advance control-plane state. Only typed submissions accepted by `noshd` can do so.
- Deterministic schema, authorization, hash, version, graph, budget, and transition checks run before semantic LLM review; review cannot override a failed invariant.
- Telemetry is off by default; any opt-in crash report excludes research content, paths, prompts, artifacts, credentials, repository names, and device keys.

## Phase 0 security tests

- Verify the daemon has no public/LAN listener and both remote endpoints connect outward.
- Verify the relay sees no plaintext test event or status-command content.
- Verify a valid status request succeeds once, a duplicate is idempotent, and expired/stale/unauthorized requests are rejected.
- Verify Pi credentials are absent from daemon persistence, browser assets, event/log output, and relay capture.
- Verify a mocked job and Pi child remain observable across GUI reconnect, and recovery records their actual post-restart state without duplicate execution.
- Verify untrusted test text cannot obtain a tool, alter scope, or become a command solely by appearing in agent-visible content.

## Release blockers outside the spike

Before remote control is released, conduct dedicated review of the exact libsodium-based construction (recommended: Ed25519, X25519, XChaCha20-Poly1305, Argon2id where supported), nonce/key lifecycle, pairing, rotation, recovery, revocation, and device-vault storage. Treat any high/critical finding, plaintext relay exposure, missing redaction, replay acceptance, path escape, or protected-branch bypass as a release blocker.
