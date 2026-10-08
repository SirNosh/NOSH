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

You need Node >=22.19, Git, and a model account:

- a **ChatGPT** (Plus/Pro) or **Claude** (Pro/Max) subscription, or
- an **API key** from OpenAI, Anthropic, Google Gemini, OpenRouter, DeepSeek, Groq, Mistral, xAI, and others.

Install the packaged CLI. It needs no pnpm, no build step, and no separate Bun install, because the package brings its own Bun for the terminal UI:

```sh
npm install -g nosh-harness
nosh setup
nosh open
```

The npm package is a small launcher. On first run it downloads NOSH from the latest [GitHub release](https://github.com/SirNosh/NOSH/releases), checks its SHA-256, and installs it under `%LOCALAPPDATA%\NOSH\harness` (or `~/.nosh/NOSH/harness`). Once a day `nosh` tells you when a newer release exists; `nosh update` installs it, and it waits while Missions, Directions, Autoresearch, Jobs, or agents are active. `nosh update --check` only reports versions, and `nosh --version` shows what is installed. Updates move forward only.

Or run from a source checkout (this needs Bun >=1.3 on PATH for the terminal UI):

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
- Bun >=1.3 for the OpenTUI client only: the npm package ships it; a source checkout needs it on PATH (the Windows installer does not install Bun).
- Git.
- A model account, connected with `nosh setup` or `nosh login`. The `pi` CLI is optional.
- WSL and NVIDIA tooling only for workloads that use them.

`nosh open` starts the daemon if needed, then runs the TUI in the current terminal. Closing the TUI does not stop the daemon or supervised Jobs. Ordinary shell processes are not automatically supervised Jobs.

## How research is organized

Everything belongs to a **Project**: one Git repository and its approved research contract.

```text
Project                    a Git repository + an approved, versioned research contract
├── Research chat          you and the Project agent: discovery, questions, staging work
├── Mission                an approved goal with success criteria and budgets
│   └── Node               one step of the Mission's plan (literature review, implementation, final review, …)
│       └── Task           one worker in its own Git worktree, checked, then independently reviewed
├── Research Direction     one question with a frozen evaluation contract (may serve a Mission)
│   ├── Baseline           the daemon measures the starting point; a reviewer accepts it
│   ├── Autoresearch       rounds of experiments against the frozen contract (can also run on its own)
│   │   └── Experiment     proposal → implementation task → evaluation Job → result → review → promote, hold, or reject
│   └── Closure            supported, refuted, or inconclusive, with a reviewed closure packet
└── Evidence               Artifacts → Evidence → Claims → paper (LaTeX/PDF export)
```

Accepted Mission work is merged onto the Mission's own branch (`nosh/mission-<id>`), so later tasks build on it and you merge one branch at the end. Failed and negative results stay in the record.

## What NOSH can do

- **Turn a repository into a Project.** Discovery asks one question at a time, finds your test and evaluation commands, and proposes a research contract for you to approve.
- **Run Missions.** A director plans the work as a graph, workers carry out each task in isolation, and every result is reviewed before it counts. Pause, steer, retry, or stop at any time.
- **Answer research questions.** A Direction measures a baseline, runs Autoresearch, and closes with a reviewed answer.
- **Search for improvements.** Autoresearch proposes experiments, evaluates each one under the same frozen contract, enforces guardrails, and promotes only reviewed winners.
- **Run your commands safely.** Workers never get a shell; they ask the daemon to run commands declared in the contract as supervised Jobs.
- **Keep evidence honest.** Code diffs, run results, sources, and reports are content-addressed Artifacts that reviewers read for themselves.
- **Show you what is happening.** Press ctrl+g in the terminal UI for a live map of missions, directions, experiments, workers, and Jobs.
- **Use any model account.** ChatGPT or Claude subscriptions, or API keys for OpenAI, Anthropic, Gemini, OpenRouter, and more.

## How NOSH works

**Models decide; the daemon executes and judges.** Agents propose work and make judgments. `noshd` owns everything else: scheduling, Git, Jobs, budgets, validation, and acceptance. Model prose never changes state by itself.

- **Threads and Episodes.** Agent work runs as bounded threads. Each step does one piece of work and returns an immutable, hashed Episode: a compact record of verified facts, decisions, references, and changed files. Threads can run in parallel, wait on each other, and start from selected Episodes instead of whole transcripts, so context stays small.
- **Typed outputs.** Every task ends with one JSON envelope of schema-checked records. NOSH prebuilds that envelope with the daemon's own facts (IDs, commits, commands, fingerprints) already filled in, so the model only writes its judgment. The host validates it and issues a receipt; that receipt, not the model's text, decides acceptance.
- **State without bookkeeping calls.** Agents never spend tool calls on bookkeeping. Starting a task is the acknowledgement. A `PROGRESS:` line in ordinary text is a progress update. The daemon commits a worker's edits itself (before every run and after the final answer), and a file is cited simply as `"artifact:<path>"`.
- **Asynchronous, parallel tool use.** Tool calls in one turn run in parallel. Long calls, such as a test run or a network read, never block the agent. If a call is not done within a moment, the agent gets a placeholder and keeps working, and the result arrives later as a message that wakes it. An agent with nothing else to do ends its turn and sleeps until the result lands. Results are only ever appended, so the prompt cache stays valid, and edits pause while a run is testing the worktree.
- **Durable programs.** Orchestration programs are declared step graphs with typed state. Their state is checkpointed after every transition and resumes after a restart.
- **Code-owned keep decisions.** Whether an experiment is kept is decided by code, not by a model: the run must be valid, every guardrail must pass, and the measured improvement must reach the contract's minimum effect. An independent review must then agree. A model can never declare its own result a success.
- **Independent review.** Reviewers are separate sessions. They judge the daemon's records (diffs, run results, Artifacts), not the worker's claims.
- **Budgets.** Every task and step has a token budget (input + output + cache writes + a tenth of cache reads). Overruns are recorded, never silent.

See [orchestration runtime](docs/ORCHESTRATION_RUNTIME.md), [structured terminal output](docs/protocols/terminal-output.md), and [TUI commands and limits](docs/user-guide/tui.md) for details.

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

`pnpm package:npm` builds `release/npm/nosh-runtime-<version>.tgz`, an npm package with the CLI, daemon, and TUI. Releases ship it as a GitHub release asset, which the `nosh-harness` launcher (`apps/launcher`) installs. To release, run `npm version patch` (it bumps the version, commits, and tags), then `git push --follow-tags`; the release workflow tests, packages, and publishes the GitHub release. The launcher itself is published to npm once and does not change between releases. Internal packages ship inside it, and third-party dependencies (including Bun) install from npm. `pnpm package:windows` clears declared compiler output directories, rebuilds, and creates an ignored release directory with CLI, daemon, TUI, packages, the Pi package, install/package helper scripts, workspace configuration, `BUILD-METADATA.json`, and public docs (including `docs/archive/` historical documents). Do not run it alongside other builds or tests. It excludes stale compiled files, private state, dependencies, and tests. It does not bundle Bun, a browser build, or relay. Repository quality/tests/schema generation are development gates; use the checkout for these, not the stripped distribution.

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
