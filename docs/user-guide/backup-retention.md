# Backup and retention policy

Run `nosh backup <project-id-or-path>` before upgrades, schema migrations, or material graph/paper changes. Keep at least three recent copies and a monthly copy on separate storage. Test restore with disposable data.

Backups contain selected-Project operational state, `.nosh` contracts, events, sessions, metadata, and `.nosh/artifacts` manifests and stored objects, paper sources and figures, integrity metadata, a Git bundle of all refs (the full committed history of every branch, not only required commits), and selected-Project daemon Jobs under `jobs/<jobId>` including `job.json` and logs. Uncommitted working-tree files outside those paths, other Projects' Jobs, and provider credentials are excluded. Committed repository content is included through the Git bundle.

Backup requires a clean Git working tree. It is rejected while the Project has non-terminal agents, Jobs, runtime threads, Missions, Directions, or Autoresearch; paused work counts as active. Backups are written to `<state>/data/backups/<projectId>-<timestamp>/`; that directory name is the backup ID. `scripts/uninstall.ps1 -RemoveData` deletes them, so keep copies elsewhere. External datasets and outputs outside the captured paths remain external.

Accepted and negative evidence, historical command records, Reviews, evaluated commits, paper sources, and manifests remain retention-protected. Do not delete stored artifacts while accepted records reference them. Sensitive content may appear in Project files and logs; credential-path exclusions are not comprehensive redaction.

`nosh backup restore <project-id-or-path> <backup-id>` schedules restore. When the daemon is running it applies the same active-work check, records the request, and shuts down. A later `nosh start` applies it. Do not change repository or data files while restore is pending. External shells are no longer tracked by NOSH, so stop their writes yourself. Do not use restore as a way to interrupt active agents or supervised Jobs.

The bootstrap credential remains separate per-user state. Old remote vaults are not imported or used by this terminal-first release; retain or remove legacy secret files only under your own backup/retention policy.
