# Installation

## Prerequisites

Install Node >=22.19.0, the pinned pnpm version, Git, and Pi. Configure model-provider authentication through Pi. Install Bun >=1.3 on PATH for `nosh open` and `nosh tui`; Node continues to run the daemon and administration commands. The Windows installer does not install Bun.

WSL is needed only for WSL Jobs. NVIDIA drivers and tooling are needed only for GPU workloads. Neither is required just to open the terminal UI.

## Repository setup

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js start
node apps/cli/dist/main.js doctor
node apps/cli/dist/main.js tui
```

## Windows distribution

Verify the release digest and provenance before running scripts. Extract the distribution to a stable location. Run `scripts/install.ps1` as your normal user. It performs a frozen dependency install/build, creates a per-user `nosh.cmd` wrapper, and normally installs a limited logon task. `-NoPath`, `-NoScheduledTask`, and `-BinRoot` support isolated installation. Open a new terminal after PATH changes.

```powershell
nosh setup
nosh start
nosh doctor
nosh project open C:\path\to\repo
nosh open
```

State lives under `%LOCALAPPDATA%\NOSH` (or the configured per-user fallback). Setup creates the bootstrap credential; do not copy it into a Project or publish it. The daemon binds only to loopback.

## Operation and removal

`nosh open` and `nosh tui` require an interactive terminal and launch the same OpenTUI client. They do not open a web page. Closing the TUI leaves the daemon and supervised Jobs running. Use your existing shell for interactive commands; those commands do not become Jobs automatically.

Back up Projects before migration or removal. `scripts/uninstall.ps1` removes the wrapper/task according to its flags. Inspect its data-preservation options before use. Clean-machine installation, native terminal behavior, and workload-specific WSL/GPU recovery still require manual validation; a successful TypeScript build is not that evidence.
