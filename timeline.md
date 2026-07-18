# NOSH implementation timeline

## 2026-07-17

- Initialized the greenfield NOSH monorepo and configured `origin` as `SirNosh/NOSH`.
- Began Phase 0–1. Chose Node 22 LTS as the development baseline because the current Pi coding-agent package requires Node 20.6 or newer. SQLite will use a stable external driver instead of Node's experimental built-in SQLite module.
- Kept section 30 decisions deferred: the Windows service wrapper, local bootstrap transport, final crypto protocol, relay retention, and optional desktop wrapper will be selected only in their respective phases.
- Added the lean monorepo foundation (`@nosh/core`, `@nosh/wire`, `@nosh/persistence`, and `noshd`) rather than scaffolding the future graph, jobs, UI, crypto, or Pi packages before their phases need them.
- Implemented and tested the first durable control-plane slice: strict wire validation, RFC 8785 canonical hashes, project-scoped SQLite events/snapshots, idempotent event commands, host registry, single-instance lock, and loopback API access. Recovery tests verify replay after reopening a Project database.
- Replaced the deprecated `@mariozechner` Pi package with its maintained `@earendil-works` successor after npm marked the former deprecated. This raised the Node 22 pin to 22.19.0, matching the current maintained Pi package's declared engine requirement.
- Added and tested a minimal native Pi package resource. It loads through Pi's own `DefaultResourceLoader`; NOSH does not parse skills itself. Full authenticated session/event execution remains Phase 2 because this development machine has no configured Pi provider session.
