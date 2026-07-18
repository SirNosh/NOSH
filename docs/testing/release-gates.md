# Release and acceptance gates

Run every command from the repository root in PowerShell. A green local suite is necessary, not sufficient: provider, physical Windows/WSL2/GPU, deployed relay, mobile, accessibility, security, and long-soak evidence cannot be inferred from unit tests.

## 1. Toolchain and clean input gate

```powershell
Set-Location C:\NOSH\NOSH
node --version
corepack pnpm --version
git status --short
corepack pnpm install --frozen-lockfile
```

Required results:

- Node is at least 22.19.0 and pnpm is exactly the version pinned in `package.json`.
- The frozen install makes no lockfile change.
- For a release, `git status --short` is empty and the commit is reviewed. A development gate may run dirty, but its artifacts are not releasable.

## 2. Static and contract gates

```powershell
git diff --check
corepack pnpm build
corepack pnpm schemas
```

Required results:

- no whitespace errors;
- all TypeScript project references compile;
- every authoritative Zod schema accepts its minimal/full fixture, rejects its missing/unknown fixtures, and regenerates a stable SHA-256 file and JSON Schema document.

`corepack pnpm schemas` currently prints `Recursive reference ... Defaulting to any` warnings for arbitrary recursive JSON values. The command must still exit zero. This warning is a known limitation of the JSON Schema exporter; Zod is the authoritative runtime validator. A new warning outside recursive `JsonValue` fields or any changed generated file without an intentional contract review fails the gate.

## 3. Automated behavior gates

```powershell
corepack pnpm --filter @nosh/orchestration-runtime test
corepack pnpm --filter noshd test
corepack pnpm test
corepack pnpm --filter @nosh/web build
```

The focused runtime suite must prove parallel bounded steps, exact per-agent cost, all-await, selective composition, session rotation/restart, foreground forks, episode-scoped skills, preventive tool intersections, program state/guards/background joins/STOP/deduplication, authorization, causal suppression/replacement, invalid references, strict typed output, loud failures, and budget overruns.

The daemon suite must prove authenticated API behavior, runtime registry routes, Mission/Direction/Autoresearch integration, exact independent Review gates, frozen Git/job/evidence lineage, restart recovery, and focus/orphan handling. The repository suite must report zero failed or skipped-by-error tests. The production PWA build must complete without unresolved imports or service-worker failure.

### Web application visual and accessibility gate

Use a disposable profile and Git repository; never point visual fixtures at a real research Project. Capture Normal, populated Mission Board, compact Mission Chat, Project drawer, inspector drawer, and remote-restricted workbench at 390, 768, 1100, 1440, and 1920 pixels.

Required results:

- the Project sidebar, inspector, and workbench toggle independently on desktop; only one sidebar drawer is visible at a time below 1100 pixels;
- Mission Board and Chat are side by side only when the central workspace is at least 900 pixels, otherwise Chat is an explicit tab;
- the Normal/Mission switch remains beneath the composer, and switching it does not change Mission lifecycle state;
- Send/Stop controls only the foreground Pi turn; Pause/Resume/Stop use the Mission control endpoint and display the acknowledged daemon state;
- Pi model and context values come from `/models` and `/agents`; unavailable values remain labeled unavailable rather than estimated;
- the phone layout exposes Chat, Mission, Agents, Jobs, and More, while unrestricted remote Terminal input remains absent;
- no horizontal page overflow occurs outside the graph, Kanban, terminal, diff, or wide-table surfaces that intentionally own scrolling;
- browser console contains no uncaught errors, failed asset loads, duplicate event warnings, or reconnect loops;
- automated axe checks have no serious or critical findings, then keyboard-only, NVDA on Windows, VoiceOver on macOS/iPhone, 200% zoom, reduced-motion, graph-table parity, and chart-table parity pass manually;
- every status includes text or an icon in addition to color, focus is visibly purple, and required text meets WCAG 2.2 AA contrast.

Retain the screenshots and accessibility reports with the commit SHA. Browser inspection is evidence for this gate only; it does not replace the typed daemon and supervisor tests above.

## 4. Supply-chain and package gates

```powershell
$env:CI = "true"
corepack pnpm release:check
Remove-Item Env:CI
corepack pnpm package:windows
```

`release:check` repeats the frozen install, full build/tests, production PWA, notices, and CycloneDX SBOM. Inspect `THIRD_PARTY_NOTICES.md` and the generated SBOM for unexpected dependencies or licenses. Inspect the Windows release directory and its build metadata. Reject a release marked dirty, built from an unreviewed commit, or containing plaintext secrets, Project data, provider credentials, `.env` files, development databases, or caches.

## 5. Short and long soak gates

Use the short run only as a smoke test:

```powershell
corepack pnpm soak -- --seconds=10
```

The release gate is 48 hours:

```powershell
corepack pnpm soak -- --seconds=172800
```

Record start/end RSS, SQLite and event growth, log size, CPU, open handles, duplicate command/operation counts, and all recovery events. Fail for unbounded growth, corruption, a pending operation that does not converge, duplicate Episode/Job side effects, or a supervisor that stops progressing without a durable blocker.

## 6. Authenticated Pi orchestration acceptance

Use a disposable real Project and the same provider/model configuration intended for release. Do not substitute the synthetic test session.

1. Start `noshd`, exchange the ACL-protected bootstrap capability for a short-lived session, and confirm the bootstrap token is rejected on ordinary API and WebSocket routes.
2. Run Nosh, Mission Director, Research Director, Librarian/Researcher, General Worker, and Reviewer sessions. Confirm provider identity, model, thinking level, role scope, tool list, and live events in the PWA.
3. Open two threads, start delayed steps concurrently, and issue `THREAD_AWAIT` with `all`. Repeat with `first_success`, `minimum_count`, and a real `deadline`. Confirm the returned IDs point to immutable Episodes and each Episode charges only its own agent.
4. Step one thread three times, rotate its Pi session between steps, then restart `noshd`. Confirm the logical thread and compact selected Episode context survive while the old transcript is not copied.
5. Compose one Episode into another thread. Inspect the outgoing prompt: it must contain the compact projection and must not contain unrelated worker transcripts or unselected Episodes.
6. Cause a successful model turn to omit `nosh_episode_submit`, submit two distinct drafts, use an unknown artifact/evidence ID, and report a path outside the repository. Each case must fail loudly and must not produce a completed Episode.
7. Exceed tool, token, and wall-clock budgets. Confirm a failed Episode, failed thread/program state, operation failure, and causal record; no later step may silently continue.

## 7. Skill and program acceptance

1. Register a prompt skill with exact role, capability, Episode activation, input/output type, `nosh_episode_submit`, and pre/postflight gates. Confirm it appears only for one Episode and the next unskilled step uses a fresh unrestricted session.
2. Apply the skill with a wrong role, missing capability, missing input Episode type, wrong output type, unavailable check, and forbidden tool. Each must fail. Prove the forbidden tool is absent from the real Pi session before execution, not merely detected afterward.
3. Apply two skills whose allowlists differ. Confirm the Pi tool list is their intersection and cannot be widened by either manifest.
4. Register an invalid program: missing target, invalid runtime instruction, undeclared output key, and background output binding must each fail before execution.
5. Run a valid program with named typed state, an equals guard, failure branch, loop, foreground step, background step, explicit `THREAD_AWAIT`, and hard budgets. Confirm every step checkpoints state and results bind only declared keys.
6. Kill `noshd` once while a foreground program instruction is pending, once after a background operation intent exists, and once after the last step checkpoint but before program completion. Restart each time. The program must resume once, join durable background work, and never falsely complete early.
7. Submit duplicate program-run requests concurrently. Confirm one execution count. Issue `STOP` and confirm durable `stopped` state. Explicitly cancel any independent child threads that should not continue.

## 8. Foreground-fork acceptance

1. From a running Mission and a running Direction, create a scoped `THREAD_FORK`. The PWA must take over with the fork overlay and the owning supervisor must stop scheduling new cycles.
2. Exchange at least two user messages. Each message must create one `episode_foreground` Episode while the isolated Pi context remains bound to the fork.
3. Cancel one fork and Finish/STOP another. The overlay must restore the prior view, the director may resume, the physical session must end, and another step on the terminal fork must be rejected.
4. Refresh the browser and reconnect during the fork. The foreground state must be reconstructed from daemon state, not browser memory.
5. Repeat from a paired external device granted only `thread.message` and `thread.stop`. Confirm the encrypted snapshot exposes the active fork but omits Episodes and full operational traces; exchange a message and Finish it remotely.
6. Revoke one thread capability, send a stale-version message, target a background thread, and replay a completed command. Each invalid action must fail before a second model action; an exact completed replay may return only its durable receipt.

## 9. Authorization and causal-hook acceptance

1. Attempt to control a thread from an inactive agent, another worker, another Project, another Mission/Direction scope, and a spoofed nullable proposer. All must fail before dispatch.
2. Attempt a replacement hook that changes Project, instruction ID, idempotency key, proposer, or targets a thread the proposer cannot control. All must fail.
3. Exercise execute, suppress, and authorized replace assignments. Inspect the intervention record for proposal, eligibility, reasons, assignment, executed instruction/result, input/output refs, state before/after, cost, latency, and immediate downstream Episode IDs.
4. Replay an identical instruction and confirm the same durable result without a second side effect or second causal projection.

## 10. Original NOSH system acceptance

The orchestration gates do not replace the normative specification's broader exit proofs. Complete all of these before production release:

- clean non-admin Windows install, ACL inspection, scheduled task, one-instance behavior, restart, backup/restore, GUI closure, uninstall, and clean package verification;
- physical selected WSL2 distribution and GPU launch, telemetry, checkpoint, process-tree cancellation, daemon crash/reattach or lost classification, and continuous logs/metrics;
- one real frozen scientific Direction/Autoresearch project and one seeded long-running Mission under induced crashes, negative results, fresh Reviews, anti-loop/focus controls, and terminal completion packets;
- user-deployed Cloudflare relay and Pages PWA with independent iPhone and Mac pairing over external networks, revocation/rekey, reconnect, stale/replayed command rejection, interrupted-command resolution, and relay plaintext capture inspection;
- WCAG 2.2 AA keyboard, focus, screen-reader, contrast, reduced-motion, desktop, tablet, and phone audit;
- independent application-security and cryptographic review, with every high/critical finding closed;
- Authenticode or an explicitly approved unsigned-distribution policy and verified release digests/attestations.

## Evidence record

For every manual/external gate retain:

- date, commit SHA, dirty status, OS/build, Node/pnpm/Pi versions;
- provider/model/thinking configuration with secrets redacted;
- device, browser, WSL distribution, driver, GPU, relay deployment, and network topology;
- exact command or scenario, expected result, actual result, raw logs/screenshots/trace IDs;
- pass/fail, defect links, rerun evidence, operator, and independent reviewer.

Do not mark a gate passed from code inspection alone. Any unexplained runtime recovery failure, integrity failure, scope escape, duplicate side effect, invalid completed record, or unresolved high/critical security finding blocks release.
