# NOSH Research

NOSH (Networked Orchestrated Science Harness) is a local-first research operating environment for reproducible ML work and one evidence-linked paper per Project. Pi is its only LLM runtime; `noshd` owns durable state, graph transitions, supervised jobs, Git provenance, and semantic remote commands.

## Current implementation

The repository contains implementation surfaces across Phases 0–8 of the normative specification:

- strict, versioned wire records and deterministic response gates;
- observable Pi Directors, three bounded worker roles, Reviewers, and handoffs;
- native/WSL2 durable jobs, experiment worktrees, immutable evaluated commits, Direction/Autoresearch trees, Mission DAGs, scheduling, and Focus Governor;
- content-addressed artifacts, claims/evidence, canonical Markdown paper workspace, and LaTeX/PDF export;
- a responsive React PWA, loopback daemon API, encrypted IndexedDB cache, Cloudflare Durable Object relay, pairing, device revocation, and signed semantic commands;
- the `nosh` administration CLI, backup manifests, release SBOM/notices, and Windows packaging scripts.

This is not yet a release-ready claim. The [adversarial specification audit](docs/SPEC_AUDIT.md) and [web-design audit](docs/WEB_DESIGN_AUDIT.md) record the remaining internal findings and unexecuted hardware, provider-account, deployed-relay, physical-device, 48-hour soak, WCAG, and security gates instead of treating implementation as proof.

The Slate/Onyx-inspired operational layer is documented in the [typed orchestration runtime guide](docs/ORCHESTRATION_RUNTIME.md). It adds persistent logical threads, immutable Episodes, foreground forks, episode-scoped skills, bounded durable programs, and causal intervention hooks beneath the existing scientific graphs.

## Develop

Use Node 22.19 or later and the pinned pnpm 10.28.0:

```powershell
corepack pnpm install --frozen-lockfile
corepack pnpm test
corepack pnpm --filter @nosh/web build
```

Generate release compliance artifacts with `corepack pnpm notices` and `corepack pnpm sbom`. Run a soak fixture with `corepack pnpm build` followed by `node scripts/soak.mjs --seconds=172800`.

## Install on Windows

From a verified release archive, run:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\scripts\install.ps1
nosh setup
nosh doctor
nosh start
```

Then initialize a Git repository as a paper-oriented Project with `nosh project open C:\path\to\repo` and open the GUI with `nosh open`. See [installation](docs/user-guide/installation.md), [CLI reference](docs/user-guide/cli.md), and [remote setup](docs/user-guide/remote.md).

## Security and license

The loopback API is not exposed on LAN interfaces; both daemon and browser connect outward to the optional user-owned relay. The relay accepts only opaque ciphertext frames and authenticated enrolled devices. Remote control exposes no arbitrary shell.

Read [SECURITY.md](SECURITY.md), [THREAT_MODEL.md](THREAT_MODEL.md), and the [security review](docs/security/review.md) before exposing a relay. NOSH is Apache-2.0; production dependency licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
