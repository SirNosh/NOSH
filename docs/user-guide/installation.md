# Installation

## From npm

`npm install -g nosh-harness` installs a small launcher. Its first run downloads the latest NOSH release from GitHub into `%LOCALAPPDATA%\NOSH\harness`, verifies its SHA-256, and runs it; it needs only Node >=22.19.0 and Git. Update with `nosh update`, which waits while any Project has active work. The rest of this page covers running from a source checkout.

## Prerequisites

Install Node >=22.19.0, the pinned pnpm version, and Git. You also need a model account: a ChatGPT (Plus/Pro) or Claude (Pro/Max) subscription, or an API key from a supported provider (OpenAI, Anthropic, Google Gemini, OpenRouter, DeepSeek, and others). `nosh setup` connects it; installing the `pi` CLI is optional. Install Bun >=1.3 on PATH for `nosh open` and `nosh tui`; Node continues to run the daemon and administration commands. The Windows installer does not install Bun.

WSL is needed only for WSL Jobs. NVIDIA drivers and tooling are needed only for GPU workloads. Neither is required just to open the terminal UI.

## Repository setup

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js open
```

`setup` is guided:

- it creates the per-user state and checks Node, Git, and Bun, printing a fix for any failure;
- if no model account is connected, it asks how NOSH should reach a model: a ChatGPT subscription, a Claude subscription, an API key (masked input), or another sign-in (GitHub Copilot, xAI, and others). Subscriptions open your browser to sign in. Credentials are stored by Pi's runtime (`~/.pi/agent/auth.json`, shared with the `pi` CLI) and NOSH never shows them;
- it starts the daemon;
- it lets you pick the default model (and thinking level) from the models Pi lists.

Non-interactive: `setup --model=provider/id:level`, or `setup --yes` to skip the choice. Change the model later with `nosh model set provider/id:level` (`nosh model list` shows the options). `nosh doctor` re-runs the checks, including Bun, connected model accounts, and the default model. Manage accounts later with `nosh login` (add one), `nosh login --list` (show providers; ● marks connected ones), and `nosh logout <provider>`.

## Windows distribution

Verify the release digest and provenance before running scripts. Extract the distribution to a stable location. Run `scripts/install.ps1` as your normal user. It performs a frozen dependency install/build (skip with `-SkipBuild`), creates a per-user `nosh.cmd` wrapper in `%LOCALAPPDATA%\NOSH\bin`, adds it to the user PATH, and installs a limited logon task that runs `nosh start`. `-NoPath`, `-NoScheduledTask`, and `-BinRoot` support isolated installation. Run `nosh setup` before the next logon. Open a new terminal after PATH changes.

```powershell
nosh setup
nosh open
```

In the TUI, press Ctrl+O to open or create a Project (or run `nosh project open C:\path\to\repo` first).

State lives under `%LOCALAPPDATA%\NOSH`; if `LOCALAPPDATA` is unset, `%APPDATA%\NOSH`, then `~/.nosh/NOSH`. It is not configurable. `config.json` holds the port and bootstrap credential; `data/` holds `host.sqlite`, per-Project databases, `logs/noshd.log`, `jobs/`, and `backups/`. Setup creates the bootstrap credential; do not copy it into a Project or publish it. The daemon binds only to loopback.

## Operation and removal

`nosh open` and `nosh tui` require an interactive terminal and launch the same OpenTUI client. They do not open a web page. Closing the TUI leaves the daemon and supervised Jobs running. Use your existing shell for interactive commands; those commands do not become Jobs automatically.

Back up Projects before migration or removal. `scripts/uninstall.ps1` removes the wrapper, PATH entry, and logon task; `-NoPath`, `-NoScheduledTask`, and `-BinRoot` mirror the installer. Repositories and state are kept by default.

> [!WARNING]
> `scripts/uninstall.ps1 -RemoveData` deletes all of `%LOCALAPPDATA%\NOSH`, including databases, logs, Job records, the bootstrap configuration, and **all local backups** under `data\backups`. Copy backups you need to separate storage first. Clean-machine installation, native terminal behavior, and workload-specific WSL/GPU recovery still require manual validation; a successful TypeScript build is not that evidence.
