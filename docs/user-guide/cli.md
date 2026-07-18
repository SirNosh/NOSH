# CLI reference

`nosh setup`, `start`, `stop`, `status`, `open`, and `doctor` administer the per-user daemon. `nosh logs` prints the last 200 daemon-log lines.

`nosh project open <path>` requires a Git repository root. It creates `.nosh` contracts/schema lock plus `docs/paper.md`, `paper.bib`, and `figures/`; registers the external SQLite database; and makes the Project current. `nosh project list` shows registered Projects and the current marker.

`nosh mission list`, `nosh mission status <id>`, and `nosh job list` inspect durable daemon state. They do not replace Mission GUI workflows.

`nosh backup <project-id-or-path>` creates a timestamped local directory containing a SQLite online backup, canonical contract/event/session/job and paper files, figure files, Git refs, required commit IDs, and SHA-256 manifest. Large artifact bytes and datasets remain by reference. Copy the resulting directory to separate storage according to local policy.

Remote commands are documented separately. No CLI or relay endpoint provides arbitrary remote shell execution.
