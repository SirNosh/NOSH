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
  - `node scripts/sync-schemas.mjs` re-copies them. It needs a built TypeScript tree (`corepack pnpm build`), because it also writes two files from the live zod schemas:
    - `refinements.json`: where each zod refinement and default applies, the hash of its source, and zod's date regexes. The Rust validator applies these, and a refinement whose hash it doesn't know fails the tests until it is ported.
    - `vectors.json`: the TypeScript verdicts on the hand-written cases in `conformance/cases.json`, and its canonical JSON for tricky inputs. The Rust tests replay them.
  - `--check` fails if the schemas or fixtures differ from the TypeScript tree.
- **`port-map.json`** records, for each crate, the TypeScript paths it mirrors and the commit it was last synced to.
  - `node scripts/drift.mjs` lists TypeScript changes since then.
  - `--check` fails when a *strict* path changed after the sync. Strict paths hold logic that no fixture captures, such as zod refinements, migrations and the terminal-output protocol.
- **Differential tests** run the same operations through the TypeScript implementation and the Rust one, and compare the results.
  - `conformance/store-scenario.json` drives both stores; the TypeScript side runs through `scripts/store-ts.mjs`.
  - Generated IDs and timestamps are masked; hashes, sequences, receipts and error messages must match exactly.
  - Runs that switch implementation partway through one database prove each side continues the other's data.
  - These tests need a built TypeScript tree.
- **CI** (`.github/workflows/rust.yml`) builds the live TypeScript branch, then runs the Rust gates, the differential tests and both checks against it.

## Checks

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
npm ci --prefix sidecar/pi-host   # the Pi sidecar the PiAdapter tests drive
cargo test --workspace
node scripts/sync-schemas.mjs --check
node scripts/drift.mjs --check
```

## Status

| Phase | Scope | State |
|---|---|---|
| P0 | Workspace, drift tracking, CI | done |
| P1 | `nosh-wire`: IDs, time, canonical JSON + hashes, schema validation | done |
| P2 | `nosh-store`: SQLite migrations, event store, host registry | done |
| P3 | `nosh-sys`: Windows paths, processes, jobs, git, artifacts, backups | done |
| P4 | Domain: graph, scheduler, skills, episodes, threads, projects, papers, research control | done (`packages/templates` is unused in TypeScript and not ported) |
| P5 | `nosh-agent`: tools, `nosh_code`, Pi sidecar | done: P5a (terminal-output protocol, V8 `JSON.parse` errors, system prompt, tool policy), P5b (async tool calls, `nosh_code` on QuickJS, session tools, network reader, terminal turns), P5c (git commit plumbing), P5d (`PiAdapter` over the Node sidecar `sidecar/pi-host`, which hosts the pinned Pi SDK) |
| P6 | `nosh-orchestration` and `nosh-server`: supervisors, HTTP, WebSocket | done — P6a done (the typed orchestration runtime: threads, Episodes, joins, skills, programs, hooks, interventions), P6b done (the daemon core: projects, chat, jobs, terminal turns, task commands and artifacts, delegation, recovery, and the bridge from Pi sidecar threads to the daemon thread), P6c done (the Mission, Direction, and Autoresearch supervisors with the research spawner, director briefs, and task templates, started and stopped by the daemon), P6d done (the HTTP and WebSocket API and `nosh daemon`; `crates/nosh/tests/api_conformance.rs` runs one API scenario against both daemons, hands a Project between them in both directions, and drives the TypeScript TUI's controller against the Rust daemon) |
| P7 | `nosh` CLI | — |
| T  | `nosh-tui` (ratatui) | — |
| P8 | Packaging into the existing `nosh-runtime` release format | — |
