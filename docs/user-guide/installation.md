# Windows installation

## Prerequisites

- Windows 11 or a supported Windows 10 build;
- Node.js 22.19 or later with Corepack;
- Git with worktree support;
- WSL2 and a selected Linux distribution for Linux/GPU experiments;
- Pi 0.80.x with provider authentication already configured.

Download a tagged Windows archive and its `.sha256` file from GitHub Releases. Verify it with `Get-FileHash -Algorithm SHA256`, extract it into a user-writable directory, and run `scripts/install.ps1`. The release is not an Authenticode-signed executable: trust comes from the published SHA-256 digest and GitHub build-provenance attestation. The installer restores the frozen pnpm graph, builds the TypeScript workspaces, creates a user-local `nosh.cmd`, and optionally adds its directory to the user PATH.

Run `nosh setup`, then `nosh doctor`. Setup places daemon state under `%LOCALAPPDATA%\NOSH`, creates a random loopback bootstrap token, and restricts the directory ACL to the current Windows user. `nosh start` launches the per-user daemon hidden and detached; GUI closure does not stop agents or jobs.

Uninstall the command with `scripts/uninstall.ps1`. Project repositories and daemon data remain by default. `-RemoveData` explicitly removes `%LOCALAPPDATA%\NOSH`; back it up first.
