# Adversarial audit against the NOSH engineering specification

Audit date: 2026-07-18 EDT  
Normative source: `C:\NOSH\NOSH_Research_Product_Engineering_Specification.md`  
Audited implementation: working tree on `codex/phase0-phase1-foundation`

## Verdict

**Not production release ready.** Implementation now exists across every Phase 0–8 area and every orchestration Phase A–F, the monorepo builds under its pinned toolchain, all 97 automated tests pass, and a recovered native scientific mini-project exercises the central Direction/Autoresearch path. This audit found no remaining reproducible high-severity implementation omission after the repairs below. The physical, cross-device, provider-authenticated, accessibility, security-review, clean-install, and 48-hour acceptance evidence required by the specification still does not exist. The separate [web-design audit](WEB_DESIGN_AUDIT.md) records the final Flexoki/PTY implementation and its unexecuted visual gates.

This is not a claim that the code is unusable. It is a refusal to treat local component tests or synthetic Pi sessions as proof of the complete Windows/WSL2/GPU/mobile product.

## Phase-by-phase result

| Phase | Implemented and verified in this tree | Remaining exit proof |
|---|---|---|
| 0 — architecture spike | Pinned Node 22/pnpm/Pi baseline, embedded Pi SDK/resource loading, native/WSL job boundary, outbound-only relay topology, architecture/security documentation. | No authenticated Pi provider child, physical WSL2 job, and external iPhone status request were observed together. |
| 1 — wire/store/daemon | 50 strict Zod schemas, generated JSON Schema documents and 250 fixtures, canonical hashing, SQLite events/projections/graph versions/receipts/operation intents, Project registry, loopback HTTP/WS with short-lived session exchange, single-instance lock, Windows logon-task installer. | Clean non-admin Windows installation, ACL inheritance, restart, uninstall, and property/fault coverage remain external. |
| 2 — Pi-native agents | Embedded sessions, Pi package resources and scoped tools, event mapping, prompt/steer/follow-up/abort/compact, Directors and three worker roles, retry gates, handoff validators, persistent finalized output and token attribution. | Authenticated provider runs, GUI session inspection, automatic long-session rotation, crash cursor recovery, and Pi package install/update/remove have not been proven. |
| 3 — jobs and provenance | Detached native/WSL supervisor, PID/process fingerprints, logs, timeout/checkpoint/cancel/recovery, scoped GPU attribution, idempotent Job launch, isolated Git worktrees, frozen commits/contracts, and reviewed promotion. | Native jobs are proven in the recovered mini-project. Physical WSL2/GPU process-tree recovery and telemetry are not. |
| 4 — Normal GUI | Responsive Flexoki React PWA with the four-region shell, Project setup, daemon queries, encrypted IndexedDB cache, Pi-backed Normal chat, Board/Graph/Timeline/Completion Mission mode, live inspector, real local PTY terminal, remote-shell restriction, typed lifecycle controls, graph table fallbacks, CSP/service worker, and lazy heavy modules. | Browser fixtures were manually inspected before the final PTY replacement, but no retained axe, screen-reader, full breakpoint, reconnect, terminal-visual, or scale acceptance suite exists. |
| 5 — Directions/Autoresearch | Durable Direction/Autoresearch projections; canonical Task, Review, closure, and completion records; exact reviewed baseline; restart-safe experiment tree; objective-normalized ranking; recovered budgets; Git/worktree/freeze/Job/Artifact/Evidence/round/promotion lineage; negative results; final Reviews. | The integrated fixture runs four native variants over two rounds, restarts SQLite after round one, and leaves no pending intent. A real scientific repository/provider and WSL2/GPU evaluation are still required. |
| 6 — Mission/Focus | Full Mission contract fields, approved graph version, durable leases/states, one active Mission, autonomous supervisor, typed Director cycle, canonical tasks and exact Reviews, deterministic postflight, retries, scheduler accounting, recovered Focus history/allocations, budget enforcement, orphan-lease repair, completion packet and final Review. | Fixtures use synthetic Pi responses. A single seeded long-running tunnel-vision Mission with authenticated models, induced crashes, jobs, and all specified focus-share policies remains unproven. |
| 7 — encrypted remote | User-owned Cloudflare Worker/Durable Object, bounded opaque frames/latest snapshot, Ed25519 challenge auth, X25519 wrapping, XChaCha payloads, Argon2id vault, terminal pairing QR, permissioned individual revocation, versioned account-key rotation with per-device sealed rekey frames, signed expiring commands, encrypted offline snapshots, fail-closed replay, and explicit local resolution of interrupted commands. | No deployed external-network iPhone/Mac pairing, rekey, reconnect, command, or plaintext-capture suite exists. |
| 8 — paper/release | Canonical Markdown/bibliography workspace, deterministic LaTeX source/export logs, Artifact/Claim helpers, online SQLite backup, CLI, notices, CycloneDX SBOM, CI/release/Pages workflows, Windows package/install scripts, license/security/user docs, pinned release gate and dirty-build metadata. | No clean-machine reference run, 48-hour soak, formal WCAG audit, or independent security/cryptography review has passed. Distribution has digest/attestation documentation but no Authenticode signature. |
| A–F — typed orchestration addendum | All ten typed operations; persistent logical threads and replaceable Pi sessions; strict immutable Episodes and compact selective context; capability-scoped local/encrypted-remote foreground PWA forks; preventive episode-scoped skill tool/capability/type gates; bounded persistent programs with named state, guards, branches, loops, durable background joins, checkpoints, deduplication, STOP and restart resume; authorized execute/suppress/replace causal hooks. | Authenticated model-selected operations, physical foreground-fork reconnect, and kill-at-every-boundary recovery remain manual/external gates. Longer-horizon scientific outcomes are derived from the event log rather than guessed in the immediate intervention record. |

## Earlier blocking findings now closed for the exercised path

- Scientific execution is now one recovered daemon path: proposal → Task Packet/acknowledgement → isolated Git worktree → frozen commit/contract → idempotent native Job → metric/log/config/environment Artifacts → positive or negative Evidence → fresh Reviews → reviewed integration promotion → round and completion records.
- Git worktree creation, freeze, promotion, filesystem writes, Artifact copies, paper replacement, and Job launch use durable canonical operation intents. Exact retries verify the external result before completion.
- Mission scheduler allocations, Focus attempt fingerprints, node attempts/leases, Pi token use, Job GPU time, wall time, disk use, and Autoresearch experiment/round/token/GPU/wall/disk use rebuild from durable state. Orphaned node owners are released or charged an attempt.
- Mission, Direction, and experiment work uses daemon-issued Task Packets, exact acknowledgements, deterministic schema/Git/command/scope/criterion/Artifact postflight, canonical Review Requests, and exact independent verdict matching.
- Mission, Direction, and Autoresearch terminal transitions require their typed completion/closure packet plus a fresh exact PASS Review. Direction baseline acceptance no longer accepts an arbitrary PASS-shaped record.
- Local clients exchange the ACL-protected bootstrap capability for a 15-minute in-memory daemon session. The PWA stores only that short-lived token in tab-scoped `sessionStorage` and deletes the bootstrap capability after exchange; direct bootstrap access to normal API routes is rejected by test.
- The PWA can author the runnable frozen evaluation contract and execution budgets, create Direction-owned or standalone Autoresearch drafts, and explicitly start, pause, resume, or stop them without API-level intervention.
- Device revocation rotates the account key/version and durably publishes an old-key-encrypted, device-targeted rekey record whose new key is sealed to each remaining X25519 public key. The revoked client is closed and cannot reconnect or unwrap the new key. Accepted commands interrupted before a terminal receipt remain fail-closed and appear in local Settings until the user records an inspected terminal resolution.

## Release-blocking internal findings

None reproduced in the final automated/static pass. This is narrower than a production security or acceptance verdict: the unexecuted external gates below can still reveal blocking defects.

## Significant non-blocking findings

- Job truth is persisted atomically in per-job JSON and Project events, not the dedicated Project SQLite Job projection described by the storage model.
- Paper export and several secondary filesystem operations do not yet use the general operation-intent journal, although paper replacement and the integrated scientific/completion paths do.
- The generated JSON Schema for recursive arbitrary `JsonValue` fields is weaker than the authoritative runtime Zod validation; top-level strictness remains intact.
- The invalid-fixture matrix covers missing/unknown fields for every schema plus semantic tests, not every enum/version/ID/cross-field mutation.
- Pi session files remain Pi-native, but startup does not fully restore provider sessions/cursors or automatically rotate long contexts through a durable handoff chain.
- Focus detects repeated work and global budget exhaustion, but the normative low-criticality per-node share of remaining discretionary Mission budget is not separately enforced.
- The Agent inspector exposes live model/context/tool/task/control state but not the full pin/history, diff/branch/resource, handoff, and compaction treatment specified for the polished inspector.
- Remote PWA Mission controls exist, but equivalent polished previews and controls for every agent/job/review/artifact operation are incomplete.
- The orchestration runtime has synthetic parallel/restart/failure coverage, but authenticated Pi tool allowlists, physical foreground-fork takeover/reconnect, and crash injection at every program checkpoint have not been observed. `STOP` intentionally stops a program interpreter; independent spawned threads require explicit `THREAD_CANCEL`.
- The Windows scheduled task/ACL commands have not been run on a clean non-admin account. Tray and keep-awake behavior are absent.
- CLI and PWA packages have no DOM-level automated behavioral tests. Their confidence comes from integration API/PTY coverage, production builds, an isolated CLI smoke run, and the browser inspection recorded in `timeline.md`.
- The PWA has two large lazy chunks of about 954 kB and 1.13 MB uncompressed; no low-end-mobile p95 result exists.
- The current audit covers a dirty working tree. Packaging now marks this state explicitly; a release must be produced from a clean reviewed commit.

## Security disposition

No known plaintext-relay path or arbitrary remote shell remains in the reviewed code. Command type, target, permission, signature, account/device scope, revocation, expiry, optimistic version, payload schema, and idempotency are enforced before dispatch. Artifact previews are bounded and encrypted, path traversal is rejected, relay snapshots are bounded, and exact completed-command replay survives restart.

This is not an independent security assessment. The browser vault/XSS boundary, local session design, per-device rekey sequence, Windows ACL inheritance, Cloudflare deployment, interrupted-command resolution UX, and exact cryptographic construction remain external review gates. Any external high or critical finding blocks release.

## Verification performed

- `CI=true corepack pnpm release:check` under the pinned pnpm 10.28.0 toolchain: frozen install, full TypeScript build, all 97 tests, production PWA, notices, and CycloneDX SBOM passed.
- `corepack pnpm schemas`: 50 generated schema documents and 250 fixtures verified.
- Recovered Autoresearch fixture: four committed native variants, minimize objective with two viable siblings, two rounds, SQLite close/reopen after round one, negative Evidence, reviewed promotion, completion, and zero pending operation intents.
- Mission/Direction fixtures: canonical packets/acknowledgements/postflight/Reviews, immutable baseline, Evidence, typed terminal packets, completion, Focus recovery, and orphaned-lease recovery.
- `corepack pnpm package:windows`: `release/NOSH-Research-0.1.0` generated successfully with commit/dirty metadata.
- `corepack pnpm soak -- --seconds=10`: 10 events, 4,096-byte SQLite database, RSS growth `-39,739,392` bytes. This is a smoke soak, not the required 48-hour proof.
- `git diff --check`: no whitespace errors; only the repository's Windows CRLF conversion warnings were emitted.

## External acceptance still required

1. Clean non-admin Windows install/remove, ACL inspection, scheduled task, one-instance behavior, hidden child environment, restart, backup/restore, GUI closure, and clean-package verification.
2. Physical selected WSL2 distribution and GPU: launch, telemetry, checkpoint, cancel process tree, daemon crash, reattach/lost classification, and log/metric continuity.
3. Authenticated Pi provider runs for Nosh, both Directors, Worker, Librarian, and independent Reviewers, including steering, compaction, handoff/rotation, crash recovery, and package install/update/remove.
4. Real scientific repository using the frozen contract and recovered Direction/Autoresearch path, plus a seeded long-running Mission that demonstrates anti-tunnel-vision behavior under induced failures.
5. User-deployed relay and Pages PWA: independently pair iPhone and Mac over external networks, revoke one device, prove the other consumes its sealed rekey and the lost device cannot, reconnect, reject stale commands, resolve interrupted commands, and confirm relay captures contain no plaintext.
6. Full 48-hour Mission soak with bounded memory, SQLite/event growth, relay cache, logs, job recovery, and no duplicate work after induced outages.
7. Formal WCAG 2.2 AA keyboard/screen-reader/mobile audit and independent security/cryptography review.

Until the retained external proofs are complete, this tree should be described as a substantial, integrated implementation and release-candidate harness—not as a production release satisfying the entire normative specification.
