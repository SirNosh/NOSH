# Release and acceptance gates

A green unit suite is necessary, not sufficient. Record the commit, runtime versions, command output, and platform for each gate. Historical browser/PWA results do not validate this terminal-first release.

## Automated gates

Run from the repository root:

```sh
git diff --check
corepack pnpm install --frozen-lockfile
corepack pnpm quality
corepack pnpm test
corepack pnpm schemas
```

Require zero failures and intentional review of generated schema changes. Runtime Zod validation remains authoritative; known recursive JSON exporter warnings do not excuse new contract errors.

Tests must retain agent/task authorization, runtime program bounds, handoff and thread state, Mission/Direction/Autoresearch transitions, independent Review, Job scope, event replay, idempotency, restore safety, and recovery invariants. Verify removed static assets, remote/device controls, directory-picker, and terminal-session APIs are unavailable. Verify loopback Origin and session-auth boundaries still apply to remaining APIs.

## Terminal client gate

Use Bun >=1.3, the supported Node daemon, a real terminal, and a disposable Project. Check:

- `nosh open` and `nosh tui` launch the same client without putting credentials in command arguments;
- missing Bun and non-TTY use fail clearly; scripting commands still work without Bun;
- help, Project selection, supported reads, and supported commands work with keyboard input;
- status and errors remain readable without color alone;
- resizing, scrolling, copy/paste, Unicode, and terminal restoration work on supported Windows and Unix terminals;
- reconnect uses bounded cursor replay and does not claim disconnected work succeeded;
- exit leaves daemon and supervised Jobs running;
- no unsupported former browser feature is presented as implemented parity.

Manual screen-reader and keyboard accessibility testing remains required. Playwright/PWA/mobile-browser gates were removed with those clients, not marked passed.

## Windows package gate

```powershell
corepack pnpm release:check
corepack pnpm package:windows
```

`release:check` runs frozen install, quality, full build/tests, clean packaging, native OpenTUI renderer smoke, isolated Windows installer/uninstaller smoke, notices, and SBOM. It requires native Windows Node and Bun. On Linux/macOS, `node scripts/release-smoke.mjs --package-only` checks clean packaging and native rendering but explicitly does not validate installation. The smoke uses a temporary package and wrapper path with PATH and scheduled-task changes disabled. Packaging clears declared compiler output directories before rebuilding; do not run it alongside other builds or tests. Inspect generated artifacts for unexpected dependencies/licenses and secrets. Reject dirty or unreviewed release metadata. The package must contain CLI, daemon, TUI compiled entries and source workspaces, not web/relay assets, browser tests, private state, caches, or dependency directories. Bun is an external prerequisite.

## Real-workload and safety gates

1. Test real authenticated Pi sessions for each used role, exact model selection, capability-scoped tools, terminal submissions, and independent Review.
2. Run bounded Missions, Directions, and Autoresearch against disposable repositories. Confirm approval, budget, scope, evaluated-commit, and Review gates fail closed.
3. Test native and WSL Jobs where supported, GPU telemetry where required, process-tree cancellation, checkpoint behavior, daemon crashes, and restart reconciliation. No unit suite replaces physical process evidence.
4. Create and restore a backup. Verify Project/database/paper/artifact integrity, selected-Project Job records/logs, exclusion of unrelated Jobs and credentials, pending-restore behavior, and rejection while active managed work exists.
5. Test clean non-admin Windows setup, state ACLs, logon task, uninstall/data retention, and release digest verification.
6. Complete a long-running soak and independent application-security review. No current document claims these gates have passed.

Remote pairing, relay crypto, PWA deployment, and managed interactive shells are no longer release features. Do not reopen network exposure to compensate for their removal.
