# NOSH terminal workspace

This is an imperative **OpenTUI** frontend. It does not use React or Pi TUI.
The Node 22 daemon stays separate. OpenTUI runs under **Bun >=1.3**.

## Launch

Use `nosh open` or `nosh tui`. Install Bun first. `NOSH_BUN` can select its executable.
Closing the UI does not stop the daemon or its supervised jobs.

For direct launch, set `NOSH_BASE_URL`, `NOSH_BOOTSTRAP_TOKEN` (or
`NOSH_SESSION_TOKEN`), and optionally `NOSH_PROJECT_ID`, then run
`bun apps/tui/dist/main.js`. Do not put tokens in shell arguments.
The CLI uses the private `NOSH_TUI_CONFIG` environment variable instead.
`launchTui(config)` is also exported and is safe to import on Node 22; it
starts a separate Bun process on that runtime.

## Commands

Type `/help` or Ctrl+P for the command list. Enter sends a message or command.
PgUp/PgDn scroll. `/quit` or Ctrl+C detaches. Escape clears the input.

- `/projects`, `/project <id>`: choose a repository.
- `/open {"path":"/repo","workingTitle":"Study","createRepository":false}`:
  register/open a project and start daemon-owned intake.
- `/models`, `/model <provider> <id> <thinking>`: choose an authenticated model.
- `/status`, `/jobs`, `/job <id>`, `/tail <id> stderr`: inspect durable work.
- `/approvals`: inspect the contract and graph proposals.
- `/approve <proposalId> <inspectedVersion>`: stage graph approval.
- `/transition <missions|directions|autoresearch> <id> <inspectedVersion> <state>`:
  stage a version-checked state action. The daemon checks legal transitions.
- `/control <missionId> <inspectedVersion> <pause|resume|stop> [safe|checkpoint|immediate]`.
- `/cancel <jobId>` and `/checkpoint <jobId>` stage job actions.
- `/confirm` applies the staged action once. `/discard` clears it.
- `/create <missions|directions|autoresearch> <JSON file path>` reads a
  daemon API payload prepared in an external editor and stages creation.
- `/paths` shows the repository and paper/contract paths. No shell is embedded.

Project contract approval is an explicit conversation with daemon intake,
not a client-side file edit. Structured research creation uses the existing
daemon API schema. Missing fields and illegal transitions are shown as errors.

## Bounds and security

The client exchanges a bootstrap token for a session and renews on HTTP 401.
Tokens stay in memory, are never rendered, and redirects are rejected.
Only HTTPS or loopback HTTP is accepted. There is no anonymous fallback.
Polling fetches one 200-event page per second, without overlapping requests.
Failures retain the cursor and retry after five seconds. Memory retains 400
events; transcript rendering retains the last 16 KB. Status/detail views are
clipped at 16 KB. `/refresh` refreshes status views; they do not poll themselves.
Switching projects clears event state, model choice, and staged actions.
Mutations have idempotency keys and are not automatically retried on transport
failure. Authentication retries reuse the exact body/key. Inspect authoritative
state before manually repeating an ambiguous failed mutation.

## Validation

`pnpm --filter @nosh/tui test` tests pure client/controller behavior on Node.
`pnpm --filter @nosh/tui build` type-checks published OpenTUI APIs.
`bun apps/tui/dist/smoke.js` tests real native rendering and resize headlessly.
OpenTUI 0.5.11 publishes Bun and Node exports; its Node native entry requires
Node >=26.4. NOSH intentionally uses Bun for the UI and keeps Node 22 for its
daemon and SQLite bindings. Windows native rendering needs a Windows smoke run.
