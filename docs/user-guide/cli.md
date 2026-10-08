# CLI reference

| Command | Purpose |
|---|---|
| `nosh help` / `--help` | Print command usage. |
| `nosh setup [--model=provider/id:level] [--yes] [--reselect]` | Guided first run: create per-user configuration and bootstrap credential (default port 4321); check Node, Git, and Bun; connect a model account if none is connected; start the daemon; choose the default model. `--yes` skips prompts; `--reselect` re-asks for the model. |
| `nosh login` / `login --list` | Connect a ChatGPT or Claude subscription (browser sign-in), another sign-in, or an API key (masked input) through Pi's runtime; `--list` shows providers, with ● marking connected ones. |
| `nosh logout <provider>` | Remove a stored provider credential. |
| `nosh model` / `model list` / `model set <provider/id[:level]> [--restart]` | Show, list, or set the default model for sessions that select none. A running daemon picks up a new model on restart (`--restart` restarts it now). |
| `nosh start` / `stop` / `status` | Administer the background Node daemon. |
| `nosh open` / `tui` | Start the daemon if needed and launch the OpenTUI client with Bun >=1.3. Requires an interactive terminal. |
| `nosh doctor` | Check Git (and `git worktree list` for the current Project), Pi CLI on PATH (optional), NOSH Pi package metadata, connected model accounts, Bun, the default model, daemon/database, schema registration, and state location. |
| `nosh doctor --wsl --gpu` | Also require WSL availability and NVIDIA telemetry for workloads that need them. |
| `nosh logs` | Print the last 200 daemon-log lines. |
| `nosh project open <path>` | Register a Git repository root, initialize research files, and make it current. Registers through the daemon when it is healthy; writes the host registry directly only when it is not. |
| `nosh project list` | List registered Projects and the current marker. |
| `nosh mission list` / `mission status <id>` | Inspect Missions in the current Project. |
| `nosh job list` | Inspect durable Jobs across all registered Projects. |
| `nosh backup <project-id-or-path>` | Create a Project backup and print its path. The backup ID is the final directory name. Requires a clean Git working tree and no non-terminal managed work. |
| `nosh backup restore <project-id-or-path> <backup-id>` | Schedule restore for the next daemon start. |

`project open` requires a Git root, not a subdirectory. It creates the contract/schema metadata and paper workspace. It does not itself run the guided model intake workflow.

The TUI does not launch a browser or embedded shell. Use its help for supported commands. There is no remote setup, pairing, revocation, or relay command. Scripting commands do not require Bun or an interactive terminal.

`setup` starts the daemon, so `doctor` afterwards reports it healthy; after a `stop`, the daemon/database check fails until `start`. WSL and GPU tools are optional unless their flags are supplied. This does not weaken workload-specific launch validation.

The CLI uses origin-less loopback HTTP under the local-user trust model, with a 300-second request timeout. Paths given to `backup` and `backup restore` are resolved to their real path before matching. There is no CLI command to list backups; use the printed path or `GET /api/backups` ([local API](../api.md)). The TUI receives daemon configuration through its child environment. Do not share the bootstrap credential or expose the API beyond loopback.

See [backup and retention](backup-retention.md) for scope and restore precautions. Closing the TUI leaves supervised work running; use explicit scoped controls or `nosh stop` as appropriate.
