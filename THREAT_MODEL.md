# Current threat model

## Scope

NOSH is a local, single-user research harness. Node runs the daemon and CLI. Bun runs the OpenTUI client. The browser/PWA, relay, remote device protocol, and daemon-managed shell are not supported surfaces.

The workstation and daemon are trusted. Same-user malware is outside this boundary: it can access local files and origin-less loopback APIs. Model output, repository content, papers, logs, and artifacts are untrusted data, not authority.

## Assets and controls

- **Local API:** binds only to `127.0.0.1` or `::1` (the CLI uses `127.0.0.1`). A request is trusted automatically only when its socket is loopback and its `Host` header is `localhost`, `127.x.x.x`, or `[::1]`. Origin-less native requests then retain local authority; a present `Origin` must equal the loopback `Host` origin. Cross-origin requests and DNS-rebinding pages (non-loopback `Host`) are rejected. Otherwise a 15-minute bearer session from `POST /api/session`, authorized by the bootstrap capability, is required. Trusted callers can start arbitrary Jobs and agents through the API. Request bodies are limited to 3 MB. Do not expose the API through a LAN binding or tunnel.
- **Credentials:** Pi owns provider authentication. Setup stores the bootstrap capability in per-user state. The TUI launcher passes configuration in its child environment, not command arguments. Diagnostics report credential presence only. Same-user process/environment inspection is still possible.
- **Scientific authority:** daemon-issued Task Packets, strict typed submissions, role and Project scope, capability checks, budgets, deterministic postflight, and independent Reviews gate state transitions. Chat prose and UI state cannot grant authority.
- **Network egress:** agents run with network disabled. Librarian tasks get a fixed HTTPS read allowlist (Crossref, OpenAlex, Semantic Scholar, arXiv, NCBI) only when the active Project contract sets `policies.network` to `network_research.allowlisted`. The default `network_user.approved` grants none (fail closed).
- **Command execution:** agents never get a shell or a generic subprocess tool. `nosh_run` executes only argv commands declared in the user-approved contract, as daemon Jobs in the task worktree. Executed code is whatever the worker wrote and runs natively with the user's privileges, so declaring commands extends trust to agent-written code until WSL2/container isolation is the default.
- **Project isolation:** canonical roots and IDs, validated worktrees, scoped records, and explicit protected-branch authorization bound filesystem and Git effects. Mission Job controls require both Project and Mission identity.
- **Durability:** event sequences, hashes, optimistic versions, idempotency receipts, and operation intents support recovery without duplicate external effects. Unknown outcomes must not be reported as successful.
- **Restore:** non-terminal agent, Job, thread, Mission, Direction, and Autoresearch work (including paused work) blocks backup and restore; backup also requires a clean Git working tree. External interactive shells are not tracked; users must stop modifying repository/data while restore is pending.
- **Backups:** include selected-Project state, paper/contract content, `.nosh/events` and `.nosh/sessions`, a Git bundle of all refs (full committed history), stored artifacts, and selected Job records/logs. Exclude unrelated Projects and credential stores. These exclusions are not content redaction: research files and logs can still contain secrets.
- **Terminal output:** untrusted titles, logs, model text, and artifacts must not become terminal control sequences or shell commands. Terminal rendering, escape handling, paste behavior, and subprocess cleanup need platform testing.
- **Supply chain:** frozen dependencies, notices, SBOM, release hashes, and provenance support review. Bun/OpenTUI add native runtime dependencies. They are not exempt from license or security review. The Windows package remains unsigned unless a separate signing policy is applied.

## Required review

Independent security review remains open. Prioritize loopback auth/Host/Origin checks, state permissions, child-environment secrecy, terminal escape injection, process-tree controls, Project/path isolation, protected Git operations, backup/restore, package supply chain, and crash boundaries. A high/critical finding, path escape, authorization bypass, or arbitrary control-plane command execution blocks release.

Removing remote and browser features reduces attack surface; it does not prove the remaining application safe. Real provider, Windows/WSL/GPU, recovery, clean-machine, and accessibility evidence is still required.
