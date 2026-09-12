# CLI reference

| Command | Purpose |
|---|---|
| `nosh setup` | Create per-user configuration and bootstrap credential. |
| `nosh start` / `stop` / `status` | Administer the background Node daemon. |
| `nosh open` / `tui` | Start the daemon if needed and launch the OpenTUI client with Bun >=1.3. Requires an interactive terminal. |
| `nosh doctor` | Check Git, Pi, credential presence, daemon/database, schemas, and state location. |
| `nosh doctor --wsl --gpu` | Also require WSL availability and NVIDIA telemetry for workloads that need them. |
| `nosh logs` | Print the last 200 daemon-log lines. |
| `nosh project open <path>` | Register a Git repository root, initialize research files, and make it current. |
| `nosh project list` | List registered Projects and the current marker. |
| `nosh mission list` / `mission status <id>` | Inspect Missions in the current Project. |
| `nosh job list` | Inspect durable Job state. |
| `nosh backup <project-id-or-path>` | Create a Project backup. |
| `nosh backup restore <project-id-or-path> <backup-id>` | Schedule restore for the next daemon start. |

`project open` requires a Git root, not a subdirectory. It creates the contract/schema metadata and paper workspace. It does not itself run the guided model intake workflow.

The TUI does not launch a browser or embedded shell. Use its help for supported commands. There is no remote setup, pairing, revocation, or relay command. Scripting commands do not require Bun or an interactive terminal.

Run `start` before `doctor`: a stopped daemon is a failed daemon/database check. WSL and GPU tools are optional unless their flags are supplied. This does not weaken workload-specific launch validation.

The CLI uses origin-less loopback HTTP under the local-user trust model. The TUI receives daemon configuration through its child environment. Do not share the bootstrap credential or expose the API beyond loopback.

See [backup and retention](backup-retention.md) for scope and restore precautions. Closing the TUI leaves supervised work running; use explicit scoped controls or `nosh stop` as appropriate.
