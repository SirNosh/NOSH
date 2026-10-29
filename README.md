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

## Releases

The `nosh-harness` launcher installs `nosh-runtime-<x.y.z>.tgz` from a GitHub release. The Rust runtime ships in that format, Windows x64 only: `nosh.exe` behind the JS stub `packaging/nosh.mjs`, with Pi as an npm dependency of the package.

```
node --test packaging/nosh.test.mjs   # the bin/nosh.mjs stub
node scripts/package-runtime.mjs      # release build (static C runtime) -> release/npm/nosh-runtime-<version>.tgz and .sha256
node scripts/rehearse-runtime.mjs     # installs that tgz in throwaway state: R0 with npm, A through the launcher, B from TypeScript 0.1.30
```

- `package-runtime.mjs` needs the sidecar's `npm ci` first; the notices read its installed packages.
- `rehearse-runtime.mjs` needs the npm registry and GitHub. It runs the published `nosh-harness@1.0.0`, or the launcher in `--launcher <dir>` (e.g. `$NOSH_TS_ROOT/apps/launcher`).
- **Prereleases:** set `[workspace.package] version` in `Cargo.toml` (`0.2.x`, above every TypeScript `0.1.x`), run any `cargo` command so `Cargo.lock` follows, and commit both on `rust-port`. Pushing the tag `rust-v<x.y.z>` runs `.github/workflows/rust-release.yml`: the Rust gate, the package, the rehearsal, then a GitHub prerelease, which `nosh update` never installs. Tagging and publishing need the owner's explicit approval.

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
| P7 | `nosh` CLI | done (`crates/nosh/src/cli` ports `apps/cli`; `crates/nosh/tests/cli_conformance.rs` runs one command script against both CLIs) |
| T  | `nosh-tui` (ratatui) | done (`nosh open` / `nosh tui` run it in-process; `crates/nosh-tui/tests/smoke.rs` ports `smoke.ts`, and `api_conformance.rs` drives the Rust daemon with it) |
| P8 | Packaging into the existing `nosh-runtime` release format | done (`scripts/rehearse-runtime.mjs` installs the package with npm, through the launcher, and over TypeScript 0.1.30) |
