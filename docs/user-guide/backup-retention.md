# Backup and retention policy

Run `nosh backup <project-id-or-path>` before upgrades, schema migrations, or material graph/paper changes. Keep at least three recent copies and a monthly copy on separate storage. Test restore with disposable data.

Backups contain selected-Project operational state, contracts, paper sources, integrity metadata, required Git references/commits, `.nosh/artifacts` manifests and stored objects, and selected-Project daemon Jobs under `jobs/<jobId>` including `job.json` and logs. Unrelated repository files, other Projects' Jobs, and provider/device/relay credentials are excluded. External datasets and outputs outside the captured paths remain external.

Accepted and negative evidence, historical command records, Reviews, evaluated commits, paper sources, and manifests remain retention-protected. Do not delete stored artifacts while accepted records reference them. Sensitive content may appear in Project files and logs; credential-path exclusions are not comprehensive redaction.

`nosh backup restore <project-id-or-path> <backup-id>` schedules restore. When the daemon is running it checks for active managed work, records the request, and shuts down. A later `nosh start` applies it. Do not change repository or data files while restore is pending. External shells are no longer tracked by NOSH, so stop their writes yourself. Do not use restore as a way to interrupt active agents or supervised Jobs.

The bootstrap credential remains separate per-user state. Old remote vaults are not imported or used by this terminal-first release; retain or remove legacy secret files only under your own backup/retention policy.
