# NOSH Research

```text
███╗   ██╗ ██████╗ ███████╗██╗  ██╗
████╗  ██║██╔═══██╗██╔════╝██║  ██║
██╔██╗ ██║██║   ██║███████╗███████║
██║╚██╗██║██║   ██║╚════██║██╔══██║
██║ ╚████║╚██████╔╝███████║██║  ██║
╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝
   autonomous research you can verify
```

NOSH (Networked Orchestrated Science Harness) is a local-first research harness:

- Model agents propose and carry out research.
- The daemon `noshd` executes, measures, and gates the results: Missions, Directions, Autoresearch experiments, supervised Jobs, and independent Reviews.
- Pi is the model runtime, Git holds reviewable research content, and SQLite holds operational state.
- The interactive client is a terminal UI built with OpenTUI (`nosh open`).

## Get started

You need Node >=22.19, Git, Bun >=1.3 (for the terminal UI), and a model account:

- a **ChatGPT** (Plus/Pro) or **Claude** (Pro/Max) subscription, or
- an **API key** from OpenAI, Anthropic, Google Gemini, OpenRouter, DeepSeek, Groq, Mistral, xAI, and others.

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup     # or `nosh setup` after scripts/install.ps1
node apps/cli/dist/main.js open
```

`nosh setup` walks a new user through everything:

1. It checks Node, Git, and Bun, with a fix for anything missing.
2. It asks how NOSH should reach a model: sign in with your ChatGPT or Claude subscription (your browser opens), or paste an API key (input is masked). Pi's runtime stores the credential (shared with the `pi` CLI); NOSH never shows it.
3. It starts the daemon.
4. It picks the default model and thinking level for research sessions (recommended: `gpt-6-luna` at `low` when your account offers it).

In the terminal UI, press **Ctrl+O** to open or create a Project. Discovery then asks one question at a time and proposes a research contract, including your repository's test and evaluation commands. On approval, NOSH commits just the contract files. See the [quick start](docs/user-guide/quick-start.md) and [installation](docs/user-guide/installation.md) guides for Windows, WSL, and the installer.

## Requirements

- Node >=22.19.0 for the daemon, CLI, and build; pnpm 10.28.0 (pinned in `package.json`).
- Bun >=1.3 on PATH for the OpenTUI client only (the Windows installer does not install Bun).
- Git.
- A model account, connected with `nosh setup` or `nosh login`. The `pi` CLI is optional.
- WSL and NVIDIA tooling only for workloads that use them.

`nosh open` starts the daemon if needed, then runs the TUI in the current terminal. Closing the TUI does not stop the daemon or supervised Jobs. Ordinary shell processes are not automatically supervised Jobs.

## What NOSH can do

| Capability | What it gives you |
| --- | --- |
| **Projects and discovery** | Open any Git repository as a Project. Discovery interviews you one question at a time, detects runnable test/evaluation commands (package scripts, pytest, Makefile), and proposes a versioned research contract. Approved contracts are committed on their own; amendments go through `/amend-contract`. |
| **Research chat** | User-directed chat with the Project's context. Steer running work with `/steer`, retry failed work with `/retry`, and approve staged actions in the TUI. |
| **Missions** | Bounded, graph-backed plans. A Mission Director decomposes the goal into tasks, workers carry them out in isolated Git worktrees, and every task result is independently reviewed before it counts. Missions can be paused safely (at the next boundary), steered durably, and resumed. |
| **Research Directions** | Bounded questions that close with an explicit disposition (supported, refuted, or inconclusive), backed by a closure packet that a reviewer checks for fidelity to the evidence. |
| **Autoresearch** | Iterative experiments compared under a frozen evaluation contract. The daemon runs the baseline and every evaluation, checks contract guardrails against the measured metrics, and only promotes results that pass review. Negative results stay in the record. |
| **Supervised Jobs and `nosh_run`** | Evaluations and the contract's declared commands run as daemon-supervised Jobs with timeouts, logs, and recovery, never as a free agent shell. Workers call `nosh_run` with a command id; the daemon executes it and records the exit code and output. |
| **Independent Reviews** | Reviewers judge from the daemon's own facts (commands run, worktree state, registered Artifacts), not from worker claims. Failed reviews are recorded and can be retried. |
| **Artifacts, Evidence, and Claims** | Outputs are registered as Artifacts with provenance; Evidence and Claims stay tied to the exact records that produced them. |
| **Paper workspace** | Research write-ups export to LaTeX/PDF, including tables, from the Project's paper workspace. |
| **Token budgets** | Every task and runtime step has a budget (input + output + cache writes + a tenth of cache reads). Overruns are flagged instead of silently continuing. |
| **Valid-by-construction outputs** | Agents fill typed JSON templates; daemon-owned facts (ids, commands, fingerprints) are filled in by the host, so models spend tokens on decisions, not bookkeeping. |
| **Any model account** | ChatGPT or Claude subscriptions, or API keys for OpenAI, Anthropic, Gemini, OpenRouter, and more, through Pi. Pick a default model and thinking level with `nosh model set`. |
| **Terminal UI and CLI** | `nosh open` for the interactive workspace; `nosh status`, `mission`, `job`, `logs`, `backup`, and `doctor` for scripting and administration. |

## Durable research workflow

A Project binds a Git root, versioned research contract, paper workspace, and operational history. Normal research chat is user-directed. Missions coordinate bounded graph-backed work. Research Directions resolve bounded questions. Autoresearch compares experiments under frozen evaluation contracts.

Every task runs in its own Git worktree. Models decide; the daemon executes and judges:

- Evaluations and Project-contract commands (`nosh_run`, opt-in through `/amend-contract`) run as supervised Jobs, never as an agent shell.
- Contract guardrails are checked against measured metrics.
- Reviewers read the daemon's own facts and the registered Artifacts rather than worker claims.

Daemon-issued tasks, scope checks, budgets, typed responses, deterministic validation, and independent Reviews gate accepted results. Model prose and client state are not authority. Artifacts, Evidence, Claims, and paper content remain tied to exact records and provenance. Failed and negative results remain part of the history.

The TUI supports chat, Project intake/selection, model selection, status, Job details/tails, staged controls and approvals, and external workspace paths. It keeps at most 400 recent events, shows the last 120 transcript entries (64,000 characters), and clips each displayed item to 16,000 characters. It has no built-in paper editor, graph canvas, artifact viewer, or foreground-fork takeover. Advanced operations remain in the local typed API and orchestration tools. See [TUI commands and limits](docs/user-guide/tui.md).

Task terminal outcomes and runtime Episode drafts use one final assistant-text JSON envelope. The host parses, authorizes, validates, and applies it; this is not provider-constrained decoding. These scoped turns replace response/review/episode submission tools, not execution or immediate acknowledgement/progress/effect tools. Normal unscoped chat retains its tools and prose. A host receipt, not model output, determines acceptance. See [structured terminal output](docs/protocols/terminal-output.md).

## Administration and scripting

```sh
nosh login                      # connect a ChatGPT/Claude subscription or an API key
nosh login --list               # providers; ● marks connected ones
nosh logout <provider>
nosh model                      # show the default model
nosh model list                 # models your accounts offer
nosh model set <provider/id:level>
nosh doctor                     # prerequisites, accounts, default model, daemon
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

Backup requires a clean Git working tree. Backup and restore are rejected while the Project has non-terminal agents, Jobs, runtime threads (including paused ones), Missions, Directions, or Autoresearch. Restore is scheduled, then applied on a later daemon start. Do not change the target repository or daemon data while restore is pending. The removed embedded-shell probe is no longer part of this check.

`pnpm delegate <plan.json>` runs bounded typed fan-out and requires `NOSH_DAEMON_URL` (for example `http://127.0.0.1:4321`) and `NOSH_BOOTSTRAP_TOKEN` in its environment; `--dry-run` prints instructions without contacting the daemon. `pnpm context:audit` estimates static prompt resource size.

## Local security

The API binds only to loopback. A loopback request is trusted automatically only when its `Host` header is a loopback name (`localhost`, `127.x.x.x`, or `[::1]`) and it either has no `Origin` (native clients) or an `Origin` matching that loopback host. This rejects cross-origin and DNS-rebinding browser requests. Other requests need a 15-minute bearer session from `POST /api/session`, which requires the bootstrap capability. See the [local API reference](docs/api.md). Setup stores a bootstrap capability in the per-user state directory. The TUI launcher passes configuration through its child environment, not command-line arguments. Do not expose `noshd` to a LAN or tunnel. This is not protection from same-user malware.

## Verification and packaging

```sh
corepack pnpm quality
corepack pnpm test
corepack pnpm schemas
corepack pnpm release:check
```

`release:check` includes native OpenTUI rendering and an isolated Windows installer/uninstaller smoke test. Run it with native Windows Node and Bun. For package/native-renderer checks only on Linux or macOS, run `node scripts/release-smoke.mjs --package-only`; this does not validate the Windows installer.

`pnpm package:windows` clears declared compiler output directories, rebuilds, and creates an ignored release directory with CLI, daemon, TUI, packages, the Pi package, install/package helper scripts, workspace configuration, `BUILD-METADATA.json`, and public docs (including `docs/archive/` historical documents). Do not run it alongside other builds or tests. It excludes stale compiled files, private state, dependencies, and tests. It does not bundle Bun, a browser build, or relay. Repository quality/tests/schema generation are development gates; use the checkout for these, not the stripped distribution.

A green unit suite does not prove real-provider behavior, Windows/WSL/GPU recovery, terminal compatibility, accessibility, backup restore on a clean machine, or independent security review.

## Documentation

- [Architecture](ARCHITECTURE.md)
- [Installation](docs/user-guide/installation.md)
- [Quick start](docs/user-guide/quick-start.md)
- [CLI reference](docs/user-guide/cli.md)
- [Terminal workspace](docs/user-guide/tui.md)
- [Local HTTP API](docs/api.md)
- [Backup and retention](docs/user-guide/backup-retention.md)
- [Orchestration runtime](docs/ORCHESTRATION_RUNTIME.md) and [structured terminal output](docs/protocols/terminal-output.md)
- [Release gates](docs/testing/release-gates.md)
- [Security policy](SECURITY.md), [threat model](THREAT_MODEL.md), and [security review status](docs/security/review.md)
- [Archived historical documents](docs/archive/) (removed browser/PWA product)

## License

Apache-2.0. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for generated dependency notices.
