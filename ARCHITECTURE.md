# Current architecture

NOSH is a terminal-first, local research control plane. The CLI and OpenTUI client are thin clients of `noshd`. Pi is the model runtime. There is no browser build, PWA, relay, device pairing, or daemon-managed interactive shell.

## Processes

- Node runs the per-user daemon and administration CLI.
- `nosh open` / `nosh tui` starts the daemon if needed and launches the TUI with Bun >=1.3. Bun is required only for OpenTUI. Configuration passes through the child environment, not secret command arguments.
- `noshd` binds to loopback. Automatic local trust requires a loopback socket and a loopback `Host` header; native clients send no `Origin`, and a present `Origin` must match the loopback host. Cross-origin and DNS-rebinding browser requests are therefore rejected. Other callers use 15-minute bearer sessions issued by `POST /api/session` with the bootstrap capability. Routes are listed in [docs/api.md](docs/api.md).
- Closing the TUI does not close agents or supervised Jobs. Shell work belongs in the user's terminal, separate from supervised execution.

## Authority and storage

The daemon owns Project registration, versioned contracts, graphs, Missions, Directions, Autoresearch, agents, Jobs, Reviews, Evidence, and typed semantic operations. Client presentation and model prose cannot mutate durable state directly.

Each Project has a Git repository and an external SQLite database. Events, projections, version checks, receipts, and operation intents carry scope and identity. Git holds research code and paper content. Artifacts are content-addressed and connect evidence and claims to exact outputs and evaluated commits.

Mutations validate first and record durable state with idempotency. External filesystem, Git, artifact, and Job actions use operation intents so retries and restart do not silently duplicate effects. Ephemeral live output is not durable authority. Bounded event replay uses a cursor and page limit; internal full replay remains available to recovery and projection code.

## Scientific runtime

Mission and Direction graphs, frozen evaluation contracts, experiment lineage, and evidence/claim graphs remain scientific authorities. Supervisors issue bounded Task Packets, enforce budgets, run deterministic postflight, and require independent Review before terminal acceptance.

Logical execution threads survive replaceable Pi sessions. Episodes compact completed steps into typed records. Skills constrain tools and pre/postflight checks. Bounded orchestration programs use explicit state, guards, joins, checkpoints, budgets, and failure branches. Program STOP does not silently cancel independent child work.

The TUI is an initial, smaller control surface. Backend capabilities do not imply complete TUI coverage or parity with deleted browser views. Use its help for supported controls; scripting uses the CLI, local API, and typed orchestration helpers.

## Jobs, recovery, and backup

Jobs run independently of UI lifetime. Native and WSL execution retain process identity, launch intent, logs, lifecycle state, and scoped controls. Mission operations may control only Jobs matching both Project and Mission. Interactive terminal commands are not converted into Jobs automatically.

Restart reconciles durable projections, pending operations, process fingerprints, leases, and runtime state without guessing unknown outcomes. Restore retains active agent/Job/thread/Mission/Direction/Autoresearch safety checks. No managed-shell activity check remains because managed shells no longer exist.

Backups contain the selected Project's database, `.nosh` contracts/events/sessions/artifacts and metadata, paper sources and figures, a Git bundle of all refs (full committed history), integrity metadata, and selected-Project daemon Job records/logs. Backup requires a clean Git working tree and no non-terminal managed work. They exclude unrelated Projects and credential stores. Content can still contain sensitive research data; exclusions are not comprehensive secret redaction. Restore is scheduled for the next daemon start.

## Release boundary

The distribution contains CLI, daemon, TUI, shared packages, Pi resources, install/package helpers, and public docs. Browser/PWA/relay bundles and Playwright wiring are removed. Windows install/uninstall remains per-user. Bun and provider credentials are external prerequisites.

See [release gates](docs/testing/release-gates.md) for validation. Unit tests do not establish clean-machine Windows behavior, physical WSL/GPU correctness, terminal accessibility, real-provider correctness, or independent security approval.
