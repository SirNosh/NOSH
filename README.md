# NOSH — Rust port

This branch (`rust-port`) holds a work-in-progress Rust port of NOSH. The TypeScript implementation on `agent/optimize-harness-and-project-intake` remains the reference and the shipping version.

The port reproduces NOSH's **contracts**: wire formats and hashes, the SQLite schema, the HTTP and WebSocket API, the on-disk layout and the release layout. The goal is a drop-in compatible daemon that the existing CLI and TUI, or the Rust ones, can use against the same data.

## Working layout

Develop this branch as a git worktree nested inside a TypeScript checkout, so the reference tree sits at `..`:

```
git worktree add NOSH-rust rust-port      # from the TypeScript checkout
echo /NOSH-rust/ >> .git/info/exclude     # keep it out of the TypeScript branch
```

Scripts and tests find the TypeScript tree through `--ts-root`, then `NOSH_TS_ROOT`, then `..`.

## Keeping up with TypeScript changes

- **`conformance/schemas/`** is a vendored copy of the TypeScript JSON Schemas and golden fixtures; `SOURCE.json` records the source commit.
  - `node scripts/sync-schemas.mjs` re-copies them.
  - `--check` fails if they differ from the TypeScript tree.
- **`port-map.json`** records, for each crate, the TypeScript paths it mirrors and the commit it was last synced to.
  - `node scripts/drift.mjs` lists TypeScript changes since then.
  - `--check` fails when a *strict* path changed after the sync. Strict paths hold logic that no fixture captures, such as zod refinements, migrations and the terminal-output protocol.
- **CI** (`.github/workflows/rust.yml`) runs the Rust gates and both checks against the live TypeScript branch.

## Checks

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node scripts/sync-schemas.mjs --check
node scripts/drift.mjs --check
```

## Status

| Phase | Scope | State |
|---|---|---|
| P0 | Workspace, drift tracking, CI | in progress |
| P1 | `nosh-wire`: IDs, time, canonical JSON + hashes, schema validation | next |
| P2 | `nosh-store`: SQLite migrations, event store, host registry | — |
| P3 | `nosh-sys`: Windows paths, processes, jobs, git, artifacts, backups | — |
| P4 | Domain: graph, scheduler, research control, episodes, threads | — |
| P5 | `nosh-agent`: tools, `nosh_code`, Pi sidecar | — |
| P6 | `nosh-orchestration` and `nosh-server`: supervisors, HTTP, WebSocket | — |
| P7 | `nosh` CLI | — |
| T  | `nosh-tui` (ratatui) | — |
| P8 | Packaging into the existing `nosh-runtime` release format | — |
