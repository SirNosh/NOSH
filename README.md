# NOSH Research

NOSH (Networked Orchestrated Science Harness) is a local-first research harness. `noshd` owns durable Project state, agents, scientific orchestration, and supervised Jobs. Pi is the model runtime. Git holds reviewable research content. SQLite holds operational state.

The interactive client is an OpenCode-style terminal UI built with OpenTUI, with a multiline composer, searchable dialogs, streamed transcript, and responsive research sidebar. `nosh open` and `nosh tui` launch it. The browser/PWA, relay, device pairing, and embedded shell have been removed. This is not a claim of full parity with the former web interface. Use the TUI's help for its implemented views and commands. Backend research contracts and safety gates remain authoritative.

See [exact Windows and WSL launch commands](docs/user-guide/quick-start.md).

## Requirements and setup

- Node >=22.19.0 for the daemon, CLI, and build.
- pnpm 10.28.0 (pinned in `package.json`).
- Bun >=1.3 on PATH for the OpenTUI client only.
- Git and Pi, with provider authentication configured through Pi.
- WSL and NVIDIA tooling only for workloads that use them.

From the repository root:

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm build
corepack pnpm test
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js start
node apps/cli/dist/main.js doctor
node apps/cli/dist/main.js tui
```

The Windows installer `scripts/install.ps1` creates a per-user `nosh` wrapper and optional logon task. It does not install Bun. After installation:

```sh
nosh setup
nosh start
nosh doctor
nosh project open <git-repository-root>
nosh open
```

`nosh open` starts the daemon if needed, then runs Bun in the current terminal. Closing the TUI does not stop the daemon or supervised Jobs. Use your normal terminal for interactive shell commands. Ordinary shell processes are not automatically supervised Jobs.

## Durable research workflow

A Project binds a Git root, versioned research contract, paper workspace, and operational history. Normal research chat is user-directed. Missions coordinate bounded graph-backed work. Research Directions resolve bounded questions. Autoresearch compares experiments under frozen evaluation contracts.

Daemon-issued tasks, scope checks, budgets, typed responses, deterministic validation, and independent Reviews gate accepted results. Model prose and client state are not authority. Artifacts, Evidence, Claims, and paper content remain tied to exact records and provenance. Failed and negative results remain part of the history.

The TUI supports chat, Project intake/selection, model selection, status, Job details/tails, staged controls and approvals, and external workspace paths. It keeps at most 400 recent events and clips displayed text to 16,000 characters. It has no built-in paper editor, graph canvas, artifact viewer, or foreground-fork takeover. Advanced operations remain in the local typed API and orchestration tools. See [TUI commands and limits](docs/user-guide/tui.md).

Task terminal outcomes and runtime Episode drafts use one final assistant-text JSON envelope. The host parses, authorizes, validates, and applies it; this is not provider-constrained decoding. These scoped turns replace response/review/episode submission tools, not execution or immediate acknowledgement/progress/effect tools. Normal unscoped chat retains its tools and prose. A host receipt, not model output, determines acceptance. See [structured terminal output](docs/protocols/terminal-output.md).

## Administration and scripting

```sh
nosh status
nosh project list
nosh mission list
nosh mission status <id>
nosh job list
nosh logs
nosh backup <project-id-or-path>
nosh backup restore <project-id-or-path> <backup-id>
nosh stop
```

Restore is scheduled, then applied on a later daemon start. Do not change the target repository or daemon data while restore is pending. Active agents, Jobs, runtime threads, Missions, Directions, and Autoresearch still block unsafe restore. The removed embedded-shell probe is no longer part of this check.

`pnpm delegate <plan.json>` runs bounded typed fan-out; `--dry-run` prints instructions without contacting the daemon. `pnpm context:audit` estimates static prompt resource size.

## Local security

The API binds only to loopback. Origin-less native clients are trusted by the local-user model; arbitrary cross-origin browser requests are rejected. Short-lived bearer sessions remain available. Setup stores a bootstrap capability in the per-user state directory. The TUI launcher passes configuration through its child environment, not command-line arguments. Do not expose `noshd` to a LAN or tunnel. This is not protection from same-user malware.

## Verification and packaging

```sh
corepack pnpm quality
corepack pnpm test
corepack pnpm schemas
corepack pnpm release:check
```

`release:check` includes native OpenTUI rendering and an isolated Windows installer/uninstaller smoke test. Run it with native Windows Node and Bun. For package/native-renderer checks only on Linux or macOS, run `node scripts/release-smoke.mjs --package-only`; this does not validate the Windows installer.

`pnpm package:windows` clears declared compiler output directories, rebuilds, and creates an ignored release directory with CLI, daemon, TUI, packages, and public docs. Do not run it alongside other builds or tests. It excludes stale compiled files, private state, dependencies, and tests. It does not bundle Bun, a browser build, or relay. Repository quality/tests/schema generation are development gates; use the checkout for these, not the stripped distribution.

A green unit suite does not prove real-provider behavior, Windows/WSL/GPU recovery, terminal compatibility, accessibility, backup restore on a clean machine, or independent security review.

## Documentation

- [Architecture](ARCHITECTURE.md)
- [Installation](docs/user-guide/installation.md)
- [CLI reference](docs/user-guide/cli.md)
- [Backup and retention](docs/user-guide/backup-retention.md)
- [Orchestration runtime](docs/ORCHESTRATION_RUNTIME.md)
- [Release gates](docs/testing/release-gates.md)
- [Security policy](SECURITY.md) and [threat model](THREAT_MODEL.md)

## License

Apache-2.0. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for generated dependency notices.
