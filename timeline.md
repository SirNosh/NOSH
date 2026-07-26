# NOSH implementation timeline

## 2026-07-17

- Initialized the greenfield NOSH monorepo and configured `origin` as `SirNosh/NOSH`.
- Began Phase 0–1. Chose Node 22 LTS as the development baseline because the current Pi coding-agent package requires Node 20.6 or newer. SQLite will use a stable external driver instead of Node's experimental built-in SQLite module.
- Kept section 30 decisions deferred: the Windows service wrapper, local bootstrap transport, final crypto protocol, relay retention, and optional desktop wrapper will be selected only in their respective phases.
- Added the lean monorepo foundation (`@nosh/core`, `@nosh/wire`, `@nosh/persistence`, and `noshd`) rather than scaffolding the future graph, jobs, UI, crypto, or Pi packages before their phases need them.
- Implemented and tested the first durable control-plane slice: strict wire validation, RFC 8785 canonical hashes, project-scoped SQLite events/snapshots, idempotent event commands, host registry, single-instance lock, and loopback API access. Recovery tests verify replay after reopening a Project database.
- Replaced the deprecated `@mariozechner` Pi package with its maintained `@earendil-works` successor after npm marked the former deprecated. This raised the Node 22 pin to 22.19.0, matching the current maintained Pi package's declared engine requirement.
- Added and tested a minimal native Pi package resource. It loads through Pi's own `DefaultResourceLoader`; NOSH does not parse skills itself. Full authenticated session/event execution remains Phase 2 because this development machine has no configured Pi provider session.

## 2026-07-17 22:12 EDT — full specification implementation resumed

- Cloned the repository into `C:\NOSH\NOSH`; confirmed `codex/phase0-phase1-foundation` is the remote default branch and the starting worktree was clean.
- Extracted the Phase 0–8 deliverables from the normative specification and established an ordered implementation plan ending in an adversarial requirements audit.
- Baseline verification initially failed because the host pnpm 11 blocked existing native/transitive lifecycle scripts. Decision: use the repository-pinned pnpm 10.28.0 through Corepack and narrowly allow the four existing packages that require lifecycle scripts (`@google/genai`, `better-sqlite3`, `esbuild`, and `protobufjs`). Broad lifecycle-script approval remains disabled.
- Verified the Phase 0/1 implementation after a clean dependency install: build and all 8 existing tests pass on Node 22.23.1.
- Scope boundary: hardware/account-dependent acceptance proofs (live Pi credentials, WSL GPU recovery, Cloudflare deployment, and physical iPhone/Mac pairing) will receive real adapters plus deterministic local fixtures; the final audit will not misrepresent those external proofs as locally executed.

## 2026-07-17 22:26 EDT — Phase 2 and Phase 3 control/runtime slices

- Phase 2 decision: keep Pi authoritative by embedding `@earendil-works/pi-coding-agent` directly. The adapter reuses Pi's model/auth runtime, resource loader, sessions, steering, abort, and compaction instead of adding any provider client or credential store.
- Added strict typed submissions for task packets, worker and Librarian completions, blockers, failures, delegation, Reviews, graph changes, handoffs, teach-back, and Director cycles. Added semantic checks that make an invalid PASS impossible and enforce exactly one schema-only correction attempt.
- Added the native Pi package extension with the eleven normative submission tools and worker, Reviewer, and Director prompt templates. Workers can request delegation but cannot spawn recursively; deterministic handoff equality controls logical ownership.
- Phase 3 decision: durable jobs use daemon-external JSON metadata plus append-only stdout/stderr files and OS process fingerprints. Native and WSL2 launches use argument arrays; WSL2 uses a fixed positional-argument shell wrapper solely to establish a recoverable process group and PID file, never to interpolate a user command.
- Added process-tree cancellation, checkpoint commands, restart reconciliation, bounded log tails, resource/GPU sampling, native/W&B/TensorBoard result ingestion, and daemon job endpoints. A native recovery test proves the process and logs outlive the initiating supervisor instance.
- Added Git worktree/branch management and immutable evaluated-experiment records. Promotion requires a frozen commit, matching contract hash, passing Review, and passing guardrails; protected-branch merge requires explicit user approval.
- Verification after these slices: TypeScript build passes; focused job and Git provenance tests pass. Hardware-dependent WSL2/GPU behavior remains for final environment-specific acceptance reporting.

## 2026-07-17 22:41 EDT — Phases 4–6 product and orchestration slices

- Phase 4 decision: implement the normative React/Vite surface with each selected stack dependency used for its stated concern (routing/query cache, Radix tabs, Zustand state persistence, React Flow graphs, dnd-kit display-only Kanban drag, CodeMirror paper editing, ECharts focus visualization, and Tailwind build styling). No generic UI kit or imagery dependency was added.
- Built distinct Normal and Mission modes, structured Direction/Autoresearch launch sheets, live agent/job panels, Direction/experiment/evidence/paper/notification/settings views, mobile semantic controls, visible keyboard focus, reduced-motion support, service worker shell caching, and AES-GCM IndexedDB snapshot caching. Normal mode deliberately filters Mission events; stored Mission UI selection survives mode switches.
- `noshd` now serves the production PWA on loopback with strict security headers and exposes the same API under `/api`. Browser WebSockets authenticate through the subprotocol header because browsers cannot set an Authorization header during the handshake; the auth token is not placed in the URL.
- Phase 5 decision: keep research invariants in the small shared graph package. Added immutable evaluation-contract hashing, accepted-baseline gates, meaningful Autoresearch duplicate checks, rooted experiment trees, parent/depth/width rules, fresh round Reviewers, frozen evaluated commits, guardrail promotion gates, retained negative states, and Direction closure gates.
- Added content-addressed artifact storage plus the claim/evidence graph. Evidence requires exact locators; unsupported claims are flagged; derivation cycles fail; accepted and negative evidence cannot be selected for cleanup; partial trailing JSONL is repaired deterministically.
- Phase 6 decision: use deterministic code for graph and focus authority. Added immutable DAG versions/diffs, cycle and stale-version checks, legal node/Mission state transitions, leases and expiry, runnable frontier/critical path, one-active-Mission policy, budget limits, fairness aging, normalized attempt fingerprints, no-progress/repetition alarms, and concrete-proof-only alarm clearing.
- Expanded the local wire registry across Project, Mission, Direction, Autoresearch, experiment, run, job, artifact, evidence, claim, and decision records, plus deterministic desktop/mobile response rendering.
- Verification: production PWA build, repository TypeScript build, and focused graph/scheduler/evidence/template tests pass. No browser screenshot or DOM QA was performed because it was not requested and the selected site workflow explicitly avoids unsolicited visual browser testing.

## 2026-07-17 22:52 EDT — Phase 7 encrypted remote control

- Security decision: pin `libsodium-wrappers-sumo` 0.7.15 because its ESM/runtime packaging works in both Node and the browser; 0.7.16 resolved to a package missing its referenced ESM payload in this environment. Remote crypto stays isolated in `@nosh/crypto` and uses Ed25519, sealed X25519 key wrapping, XChaCha20-Poly1305, and Argon2id.
- Implemented one-time expiring pairing capabilities, locally generated per-device signing/agreement keys, verification codes, per-device permissions and revocation, password-encrypted local vaults, signed/expiring/idempotent remote commands, optimistic-version checks, and replay receipts.
- Added a Cloudflare Worker with a SQLite-backed hibernatable Durable Object. Its accepted frame shape contains only frame/device routing metadata, kind, and ciphertext; unknown fields—including attempted plaintext—close the connection. The relay provides bounded ciphertext catch-up, latest encrypted snapshots, presence, public device metadata, pairing exchange, and revocation.
- Added outbound relay clients for `noshd` and the PWA with sequence cursors and reconnect/catch-up behavior. Relay disconnection never changes local daemon, graph, agent, or job execution.
- Added the PWA device-pairing form. Device keys and Argon2id vault encryption occur only after a user action and load as a separate bundle; the password never enters a request. Windows must still approve the presented device and verification code.
- Replaced Node-only UUID and SHA-256 helpers in shared core/wire code with standards-based UUID and pinned browser-safe Noble hashing so the exact wire registry can be used by both daemon and PWA.
- Added the tag-gated GitHub Pages release pipeline with pinned Node/pnpm setup, frozen install, production build, published SHA-256 manifest, and protected Pages deployment permissions.
- Verification: repository TypeScript build, sodium pairing/envelope/vault tests, and opaque relay-frame tests pass. Physical iPhone/Mac pairing and a deployed Cloudflare/GitHub Pages topology remain external acceptance proofs.

## 2026-07-17 23:05 EDT — Phase 7 trust boundary and Phase 8 paper start

- Tightened remote authority so every semantic command has one fixed target type, permission, and strict payload schema; an enrolled device cannot relabel a weaker capability as a stronger operation.
- Added account scope, project/target scope checks, Ed25519 relay challenge authentication, one-use challenges, full-envelope account-key encryption, encrypted acknowledgements, persistent command acceptance/completion/failure events, and daemon dispatch for bounded Mission, agent, job, Review, and artifact operations. Arbitrary shell remains absent.
- Completed both halves of pairing: the browser can present device public keys, wait for Windows approval, unwrap its sealed account key, and replace the key-only browser vault with an Argon2id-encrypted paired vault. Daemon registry changes are saved through its encrypted remote vault.
- Phase 8 paper decision: canonical Markdown remains the only editable paper source. Export writes deterministic LaTeX, copies the bibliography, records the exact `latexmk` command/log, and hashes Markdown plus bibliography with SHA-256. A missing TeX installation yields an explicit warning and still preserves the generated source.
- Verification: the repository builds; crypto, relay, daemon, evidence/paper tests pass; and the production PWA builds with heavy graph/editor/chart/crypto dependencies isolated into lazy chunks.

## 2026-07-17 23:25 EDT — Phase 8 administration, release, and schema hardening

- Added the exact public `nosh` command surface. The CLI creates a per-user protected state root, launches/stops the hidden daemon without exposing its bootstrap token in process arguments, initializes a validated one-paper Project contract/schema lock, inspects Missions/jobs, administers remote devices, and prints secret-free diagnostics.
- Backup decision: use SQLite's online backup API and copy only canonical, secret-free Project/paper/session manifests plus Git refs and required commits. Large datasets/artifacts remain referenced; the backup manifest hashes every included byte. An isolated smoke run proved setup, start, Project initialization, registration, backup, listing, and graceful stop.
- Added deterministic LaTeX/paper export tests, Windows install/uninstall scripts, CI and tagged release workflows, GitHub build provenance, CycloneDX SBOM generation, dependency-notice generation, a configurable soak fixture, release documentation, and an explicit digest-based (not Authenticode) Windows distribution trust model.
- Schema hardening decision: retain Zod as the write-boundary implementation but generate pinned JSON Schema 2020-12 documents and golden fixtures from the same local registry. All 42 registered schemas now have minimal/full valid, missing/unknown invalid, and canonical SHA-256 fixtures; a test fails if runtime validators, URI documents, or hashes drift.
- External release proofs remain intentionally separate: authenticated Pi providers, physical WSL2/GPU recovery, deployed phone/Mac relay acceptance, a full 48-hour soak, independent cryptography review, and formal WCAG 2.2 AA audit still require their target environments.

## 2026-07-17 23:42 EDT — adversarial integration repairs

- The adversarial pass found that Mission/Direction/Paper/Evidence GUI surfaces were visually complete but still used seeded research content. Decision: this was not acceptable phase completion. Added SQLite entity projections and immutable graph-version rows, with event + projection + graph + idempotency receipt committed in one transaction and recovered after restart.
- Added daemon-owned Mission, Direction, Autoresearch, graph-mutation, record-query, and paper save/export APIs. Mission and Direction transitions now enforce optimistic entity versions and legal state machines; activating a second Mission in one Project is rejected. The GUI now creates and renders these durable records and real graph nodes rather than presenting sample research as authority.
- Remote Mission commands now use entity projection versions and run legal pausing/paused, stopping/stopped, resume, and steer gates. Persistent command receipts still prevent replay after daemon restart.
- The Pi audit found process-global Project/attempt environment variables in the package extension were unsafe for concurrent embedded Projects. Embedded sessions now receive Pi `customTools` whose Project, attempt, and actor scope is captured from daemon-issued session options. Package skills/prompts still load, extensions are not duplicated, and `noshd` rejects Project/actor scope mismatches before schema validation.
- Verification after repair: TypeScript build, production PWA build, daemon integration tests, projection recovery/atomicity tests, and session-bound Pi tool tests pass.

## 2026-07-17 23:59 EDT — remote PWA end-to-end repair

- The adversarial pass found that browser pairing persisted an encrypted device/account vault but never unlocked a live remote session, and React queries still required the loopback bootstrap token. Added explicit vault unlock, encrypted snapshot hydration, remote query routing, signed Mission pause/resume/stop commands, live acknowledgement timeouts, offline rejection, and remote-aware query/event subscriptions.
- Corrected command acknowledgement correlation on rejection and returned the authoritative Mission projection version instead of the unrelated Project event sequence. Remote lifecycle buttons now issue one semantic command; only the local API performs the two legal intermediate state transitions itself.
- Bounded relay snapshots below the opaque one-megabyte frame limit, omit the local SQLite path, and degrade to a smaller events-only snapshot when research content exceeds the safe plaintext budget.
- Hardened paper saves so a replayed idempotency key cannot overwrite canonical Markdown with different content; same-key retries return the original event. File replacement occurs before the persistent receipt and uses a same-directory temporary file.
- Verification: production PWA, daemon, persistence, and relay builds pass; persistence, daemon integration, and relay opaque-frame tests pass. A deployed relay/browser/device topology is still an external acceptance proof.

## 2026-07-18 00:15 EDT — executable Normal and Mission paths

- The adversarial pass found Normal mode's composer discarded messages. Added a daemon-owned Pi-native `nosh` conversation path with idempotent user-message events, live Pi streaming/tool events, queued follow-ups, and a persistent finalized assistant-text event. Only assistant text is persisted for display; hidden reasoning content is not copied into NOSH events.
- Corrected structured submission identity: canonical submissions now retain the daemon-bound Mission/Direction/agent scope, allow distinct acknowledgement/progress/completion records in one task, and deduplicate identical records by content hash instead of incorrectly permitting only one record per task.
- Added the daemon Mission Supervisor. It continuously runs typed Mission Director cycles, deterministically schedules a ready node, leases it, requires a structured worker completion, starts a fresh independent Reviewer, applies bounded retry/focus blocking, and invokes a separate final Mission Reviewer before automatic completion. Prose-only completion cannot mutate node state.
- Added durable Mission and Direction node transitions. A fixture integration test now exercises Director → worker → independent Review for every required branch plus final Mission Review and proves the Mission completes only after all node projections are accepted.
- Tightened Direction/Autoresearch boundaries: Direction baselines require an immutable commit, matching frozen contract, and stored PASS Review; Direction-owned Autoresearch inherits that contract, requires the reviewed baseline, rejects duplicate idea-family fingerprints, and is budget-bounded. Direction closure now requires baseline, graph/execution completion, scoped evidence, and PASS Review.
- Verification: daemon and production PWA builds pass; daemon integration and autonomous Mission fixture tests pass.

## 2026-07-18 00:26 EDT — final hardening and adversarial verdict

- Expanded the durable Mission contract with deliverables, deterministic success criteria, non-objectives, assumptions, budgets, approval boundaries, external-action restrictions, pause policy, final rubric, and the approval-frozen graph version. Paused Missions now retain exclusive Project control.
- Added a pre-Reviewer Mission completion audit that rejects required-node gaps, unsupported/unqualified Claims, missing Evidence references, and unresolved Artifact hashes. The final Reviewer receives the approved contract rather than only the objective.
- Fixed remote replay ordering so signature, enrolled-device permission, revocation, and expiry are always checked before idempotent replay; exact durable envelope receipts now survive daemon restart, while same-key/different-command reuse is rejected. Added executable bounded Review responses and encrypted Artifact previews, path containment, and relay-safe preview sizing.
- Bounded the relay to its latest encrypted snapshot instead of retaining every historical snapshot, made Job launch idempotent by stable Job specification, and installed a per-user Windows logon task wrapper in the Windows installer.
- Regenerated all wire documents/fixtures after protocol tightening. Full repository build and all 75 automated tests pass; the production PWA and relay builds pass. The PWA main bundle is 582.89 kB uncompressed, with graph/editor/chart/crypto payloads split into lazy chunks.
- Wrote `docs/SPEC_AUDIT.md`. Adversarial verdict: implementation surfaces exist across Phases 0–8, but the product is not release ready because the integrated Direction/Git/job/evidence path, durable external-operation recovery, fully durable Mission policy accounting, complete deterministic postflight, and multiple external acceptance gates remain open.

## 2026-07-18 00:38 EDT — Direction loop and final release verification

- Added the daemon-owned Direction Supervisor. Active Directions now run a schema-gated Research Director cycle, deterministic frontier selection, durable lease/work/postflight/review transitions, bounded retries, worker or Librarian execution, fresh independent Reviewers, reviewed immutable baseline acceptance, and a separate final Direction closure Review.
- Extended the autonomous integration fixture to prove Direction Director → baseline worker → independent Review → scoped Evidence → remaining graph → final Review. The test verifies the immutable baseline commit, accepted required nodes, Evidence scope, and terminal `closed` projection.
- Adversarial distinction: this closes the missing first-class Direction graph loop, but not the larger Phase 5 recovery proof. Git worktrees, idempotent jobs, Autoresearch rounds, ExperimentTree state, Artifact provenance, and graph proposals remain separate components rather than one daemon transaction/reconciliation path.
- Release hardening found newly published transitive packages rejected by the active minimum-release-age policy. Pinned the immediately preceding Wrangler/Miniflare line and browser compatibility table instead of weakening the policy. Migrated the existing six-package install-script allowlist to pnpm's explicit `allowBuilds` map and fixed the pnpm 11 `sbom` command collision by invoking the repository's deterministic generator with `pnpm run sbom`.
- Final verification: `release:check` passes a frozen, policy-verified install; repository build; all 76 tests; production PWA; notices; and CycloneDX SBOM. Windows packaging succeeds. A 10-second soak produced 10 events, a 4,096-byte SQLite database, and no observed RSS growth; the normative 48-hour soak remains an external release gate.

## 2026-07-18 00:56 EDT — durable recovery and Mission policy hardening

- Added a SQLite operation-intent journal with canonical request hashes and restart reconciliation for paper replacement and Job launch. The daemon now verifies the external result before completing the durable intent, so exact retries are idempotent and interrupted operations are visible and recoverable.
- Persisted Mission token, wall-clock, and `.nosh` disk consumption now participates in scheduling and terminal completion checks. Scheduler attempt allocations and Focus retry state are rebuilt from durable events, and Focus attempts are recorded before the next retry can be issued.
- Began replacing trust in worker prose with daemon-issued Task Packets: clean Git/branch/HEAD preflight, explicit lease and budget, schema-valid task acknowledgement, deterministic command/criterion/artifact postflight, and schema revalidation of Review verdicts. The stricter fixture path typechecks; autonomous integration verification is still in progress.

## 2026-07-18 01:24 EDT — canonical Review contracts and recovered Autoresearch lineage

- Completed canonical Mission Task Packet and Review Request enforcement. A PASS verdict now has to match the daemon-issued Review/Request IDs, reviewer identity, target/version, required criteria, and required Artifact/Evidence inputs; daemon-generated reference values were corrected to the wire grammar. The full daemon suite passes this path.
- Added Mission Job attribution for Mission, Direction, and Autoresearch scope plus explicit GPU-use declarations. Durable Job lifecycle events now debit elapsed GPU seconds to the owning Mission instead of reporting a constant zero.
- Added the daemon Autoresearch Supervisor and a restart fixture that executes a Direction-owned two-round experiment tree: distinct proposals, isolated Git worktrees, typed implementation packets, frozen commits, native Jobs, metric/log/config/environment Artifacts, positive and negative Evidence, fresh experiment/round/closure Reviews, reviewed promotion into an isolated integration branch, completion packet, and terminal projection. The SQLite store is closed and reopened after round one; the reconstructed coordinator completes without pending external-operation intents.
- Git worktree creation, experiment freeze, promotion, filesystem writes, Artifact copies, and Job launch now use durable intents with verified idempotent external operations in the integrated path. Exact worktree/freeze/promotion retries are safe; a repeated integration-worktree reconciliation bug found by the fixture was fixed by keeping the operation result immutable.

## 2026-07-18 01:44 EDT — terminal invariants and budget-complete Autoresearch

- Direction work now uses the same canonical Task Packet, exact acknowledgement, deterministic Git/command/artifact postflight, and fresh matching Review Request/Verdict contract as Mission work. Mission, Direction, and Autoresearch terminal transitions independently reject an unmatched standalone PASS record.
- Restart reconciliation now releases or fails orphaned Mission/Direction leases instead of leaving nodes permanently working. The recovered Autoresearch fixture recreates its coordinator after round one and completes from durable records with no pending Git, filesystem, Artifact, or Job intents.
- Closed an adversarial metric bug: round winners are ranked by objective-normalized improvement, not raw score, so minimize contracts behave correctly. The fixture now runs four variants across two rounds, proves the lower valid sibling wins, retains negative Evidence, and performs reviewed promotion.
- Autoresearch now reconstructs and enforces model-token, GPU, wall-clock, disk, experiment, and round use before issuing more work; a separate test proves recovered token exhaustion blocks without creating a proposal. Mission wall time now begins at the durable running transition instead of draft creation.
- Current verification: all nine daemon tests pass, `noshd` typechecking passes, and Windows release packaging succeeds. The repository-wide release gate and updated adversarial spec audit remain the final checks.

## 2026-07-18 01:56 EDT — adversarial closure-packet repair

- The adversarial pass found that Mission and Direction terminal transitions required deterministic gates and fresh exact Reviews but did not persist their normative typed completion/closure packets. Added daemon-built schema-valid packets with content-addressed summary and reproducibility Artifacts; filesystem writes and Artifact copies use the durable operation journal, and terminal state guards now reject missing packets.
- Tightened Direction baseline acceptance so an arbitrary PASS-shaped record cannot authorize a baseline. Acceptance now requires an exact canonical Review Request/Verdict pair for the baseline node; the recovered Autoresearch fixture was upgraded to prove that contract.
- Remote command replay now fails closed when a prior accepted command has no durable completed/failed outcome, and startup recovery reports list such uncertain commands instead of falsely acknowledging them as completed. Automatic resolution of inherently ambiguous external side effects remains a release-audit item.
- Re-pinned all nested release scripts through Corepack. The complete frozen release gate passes under the declared pnpm 10.28.0 toolchain with 83 tests, and the 10-second soak and Windows package both pass.

## 2026-07-18 02:19 EDT — complete local launch and remote recovery controls

- Replaced direct browser use of the daemon bootstrap capability with a one-time exchange for a random 15-minute tab session. Normal HTTP and WebSocket routes reject the bootstrap token; the PWA deletes it from persistent browser storage after exchange.
- Completed the PWA research launch path: structured frozen evaluation contracts, native/WSL runner selection, command/result/seeds/failure rules, all Autoresearch budgets, Direction-owned execution drafts, and explicit start/pause/resume/stop controls.
- Revocation now rotates the remote account data key and key version. Remaining devices receive durable rekey envelopes sealed to their individual X25519 public keys, the lost device is rejected and cannot unwrap the new key, and stale command key versions are rejected.
- Added a local, durable workflow for accepted remote commands interrupted before a terminal receipt. Settings exposes each uncertain command and requires an inspection note before recording `applied` or `not_applied`; replay remains fail-closed until that resolution exists.
- Focused crypto, relay, daemon, web, and CLI typechecks pass. Crypto and daemon suites pass; the full release gate is next.

## 2026-07-18 02:30 EDT — final adversarial rekey repair and release gate

- Re-read the normative pairing flow and replaced the temporary rotate-all policy with individual revocation. The relay durably orders old-key-encrypted rekey frames whose new account key is independently sealed to each remaining device; the revoked device loses relay authentication and cannot unwrap the next key version.
- Moved browser sequence acknowledgement after frame decryption, rekey persistence, and application. This removes the crash window where a client could persist a cursor past a rotation frame before saving the new key.
- Added the required one-time Windows terminal QR using `qrcode-terminal`, a single direct runtime dependency. Its payload contains only public relay routing, the expiring capability, and expiry; the short verification code and local approval remain separate.
- Final verification passes: frozen install under pnpm 10.28.0, full TypeScript build, all 83 tests, production PWA, notices, CycloneDX SBOM, schema/fixture generation, Windows packaging with dirty metadata, `git diff --check`, and the 10-second smoke soak. The adversarial audit records no reproduced internal high-severity omission and retains all unexecuted physical/external acceptance gates.

## 2026-07-18 08:36 EDT — typed orchestration runtime contracts and package boundaries

- Mapped the Slate/Onyx-inspired operational graph onto the existing NOSH EventStore instead of adding a second database or changing the scientific Mission, Direction, experiment, or claim/evidence graphs. Logical threads and immutable episodes use durable projections plus their complete event-sequence trace; Pi sessions remain replaceable transports.
- Added strict wire contracts for all ten runtime instructions, execution threads, episode drafts/episodes, skill manifests, bounded orchestration programs, and causal intervention records. Added only the ID prefixes needed by those records and exposed one Pi submission tool for semantic episode drafts.
- Added the four requested focused packages: `episodes`, `threads`, `skills`, and `orchestration-runtime`. The initial runtime covers open/step/fork/await/compose/pause/cancel/skill/direct-action/stop, session rotation, compact context rendering, bounded programs, capability checks, repository-scoped changed-file validation, durable causal hooks, and program/skill persistence without new third-party dependencies.
- Decision: episodes are derived and validated by the daemon from the bounded event trace; a missing semantic draft produces a deterministic minimal fallback rather than making trace persistence depend on model compliance. Artifact/evidence references still fail closed when unknown.

## 2026-07-18 08:49 EDT — Slate/skill-chaining/Onyx alignment pass

- Read the three supplied Random Labs essays. Their relevant constraints are: one bounded action per thread episode; reusable thread context synchronized through episode values rather than prose messaging; background fan-out plus explicit joins; episode-scoped skill activation; blocking conversational forks for interactive skills; programs with static and runtime semantics; persistent named typed state; strict agent output completion gates; bounded control flow/budgets; and loud, traceable errors.
- Corrected the initial fallback decision: a successful Pi step now must submit a schema-valid `episode-draft`; omission is a loud runtime failure. A transport/model failure still produces a durable failed Episode for diagnosis, then throws so programs can take a failure branch. Skill prompt/tool context is cleared at the episode boundary.
- Added the executable `nosh_runtime_instruct` tool and actor/scope/parent authorization. Director roles may orchestrate within their bound scientific scope; other workers may control their own logical thread and children but cannot hijack unrelated threads. Runtime tool calls return the typed instruction result instead of merely persisting prose.
- Strengthened programs from ordered prompt records into statically checked, bounded programs with declared named state, typed state values, foreground/background steps, result bindings, durable per-step checkpoints, cumulative token/tool/wall budgets, deterministic instruction identities, guarded branches, restart-resumable program state, and loud failure state. No arbitrary TypeScript VM or new dependency was added; NOSH uses the smaller instruction interpreter required by its spec.
- Added active-instruction ownership and deterministic retry recovery so duplicate calls share one promise, competing steps cannot corrupt a winner, stale Pi sessions rotate after restart, and an Episode persisted just before a crash is reattached idempotently to its logical thread.

## 2026-07-18 09:00 EDT — adversarial orchestration hardening

- Corrected concurrent Episode accounting so each bounded action charges only events from its bound Pi agent; the global sequence range remains available for interleaving/audit context. Added exact token/tool assertions under parallel execution.
- Closed replacement-hook scope escalation by making the project, instruction, idempotency key, and proposer envelope immutable and re-authorizing every replacement before dispatch.
- Enforced skill activation episode conditions and intersection semantics for multiple skills' tool permissions, persisted postflight and budget overruns as failed Episodes, and verified Episode hashes whenever records are read.
- Added durable program STOP state, duplicate program-run coalescing, automatic restart of programs left in `running`, plain named-state guards, strict static program rejection, and daemon API coverage for thread/skill reads and registration.
- The focused runtime and daemon suites pass after these repairs. Schema/fixture generation also passes; the existing generator still warns that recursive arbitrary JSON fields are weaker in exported JSON Schema than in authoritative Zod runtime validation.

## 2026-07-18 09:35 EDT — terminal-state and remote foreground adversarial pass

- Tightened terminal and concurrent thread behavior: an in-flight fork/pause is merged at the Episode boundary, terminal threads cannot reopen, active child forks block their parent, restart-aware awaits cannot return stale Episodes, and token use is attributed as a per-turn delta rather than a cumulative Pi-session total.
- Made program/skill caches Project-scoped, prevented recursive active program application, joined durable spawned intents before completion, bounded named state size/keys, and rejected malformed/duplicate control-flow and await references during static validation.
- Closed the remaining Phase C integration gap by exposing only active foreground-fork projections in encrypted remote snapshots and adding only two semantic remote commands: `thread.message` and `thread.stop`. Both use the existing signature, device capability, expiry, Project ownership, optimistic version, payload-schema, and durable replay boundary; background threads and arbitrary runtime instructions remain unavailable remotely.
- Decision: complete Episode traces stay local. A remote device can converse with or Finish a visible foreground fork, but it cannot enumerate historic threads, Episodes, program state, or invoke a generic tool/shell surface.
- The final privacy cross-check found that the older generic remote event feed would otherwise forward runtime projections and submitted Episode drafts even though the new summary snapshot omitted them. Remote publication now filters `runtime.*`, Episode-draft, and runtime-instruction records in both live events and snapshot history while still refreshing the bounded active-fork projection.
- Final verification after that repair passes: frozen install, full TypeScript build, all 101 tests, production PWA, notices, CycloneDX SBOM, 50 generated schemas/250 fixtures, Windows packaging, `git diff --check`, and the 10-second smoke soak. Physical/provider/deployed-relay/WCAG/security/48-hour proofs remain explicitly unclaimed.

## 2026-07-18 10:22 EDT — minimal GUI and clean shutdown repair

- Fixed `nosh stop` by initializing the short-lived daemon session cache before top-level command routing. The isolated lifecycle now completes `start → running → stop → stopped` without the prior temporal-dead-zone failure.
- Removed only redundant or inert GUI chrome: the duplicate Normal/Mission header switch, the Normal-mode contract banner and live-work drawer, placeholder review/mission/job/filter buttons, and the unused compact agent-list variant. Real research, Mission lifecycle, navigation, and settings actions remain reachable.
- Built the CLI and web app, connected the production GUI to the isolated clean Project, inspected Normal and Mission in the in-app browser, then closed the test tab and cleanly stopped the daemon. No dependency or component abstraction was added.

## 2026-07-18 13:37 EDT — Flexoki web-app baseline

- Replaced the decorative dashboard shell with the four-region Project / workspace / inspector / workbench layout from the design baseline.
- Kept `noshd` authoritative: Mission steering, pause, resume, and stop now use typed daemon commands; UI selection and Kanban drag remain display-only.
- Exposed Pi's actual model registry and session context usage instead of inventing model or token values.
- Removed Tailwind and ECharts because the new UI uses authored Flexoki CSS and no longer needs those dependencies.
- Kept Terminal honest: the workbench reports that a PTY bridge is not configured rather than rendering a fake shell. Remote shell remains disabled.

## 2026-07-18 13:54 EDT — responsive and visual QA repairs

- Browser-tested Normal, compact Mission Chat, wide Board-plus-Chat, phone Mission control, and mutually exclusive phone drawers against an isolated daemon Project.
- Fixed the central-width Mission breakpoint, wide/compact tab semantics, phone-specific panel persistence, composer wrapping, disabled primary controls, and stale encrypted-cache rejection found during visual inspection.
- Added self-hosted variable Geist Sans and Mono from two dependency-free OFL font packages; avoided the `geist` framework package because it pulled in an unused Next.js runtime.
- Added positive local-API coverage for Mission pause and resume and documented the web visual/accessibility acceptance gate.

## 2026-07-18 14:12 EDT — real terminal and Project setup closure

- Replaced the temporary local-Terminal notice with daemon-owned `node-pty` sessions and a minimal xterm client. The implementation has authenticated WebSocket attachment, bounded replay, PowerShell/WSL profiles, multiple tabs, Project-root working directories, resize/input handling, and explicit close; the remote PWA still receives no shell route.
- Moved Project initialization into one `@nosh/evidence` function shared by CLI, daemon, and GUI. The New Project sheet now creates or opens a Git root and writes the Project contract, schema lock, paper workspace, and database registration without duplicating initialization logic.
- Persisted the selected Pi model per Project and added correct static font MIME handling. Decision: do not add a terminal/session abstraction layer beyond the one daemon owner and two small React components.
- Focused daemon coverage now proves both Project initialization and an authenticated PowerShell PTY round trip. The frozen-install full release check passes the repository build, 97 tests, production PWA, dependency notices, and SBOM.

## 2026-07-26 20:20 EDT — token and prompt-cache optimization

- Kept SQLite events and immutable Episodes as the only durable truth. Continuation now uses a generated recent-first Episode projection with a 16,000-character budget; omitted selected Episodes remain fetchable by ID, so no parallel model-maintained summary document was added.
- Put invariant step rules before volatile objective/context, replaced large successful submission echoes with a fixed receipt, and added a compact role system prompt. OpenAI cache affinity is stable per Project, role, and tool profile rather than per Agent/task/request.
- Added cache read/write/input/output counters to durable Pi completion events plus deterministic `delegate` fan-out/join and `context:audit` scripts. Decision: reuse the existing typed orchestration runtime instead of adding another scheduler, queue, or truth store.
- Delegated workers are explicitly stopped after the join, with best-effort cleanup on failure, so idle model sessions do not remain resident.
