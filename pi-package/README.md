# NOSH Pi package

This package is the sole model-runtime resource package for NOSH Research. It contributes worker/Reviewer/Director prompt templates and the `nosh-control` skill to Pi. The bounded NOSH tools are injected into each session by `@nosh/pi-adapter`, not by this package.

`noshd` loads this directory automatically for its embedded Pi sessions through an in-memory package setting; no manual Pi package installation is needed. The compatible Pi SDK is pinned by the daemon dependency (`@earendil-works/pi-coding-agent` 0.80.x). `nosh doctor` checks that the `pi` CLI is on PATH, that this package's metadata exists, and provider-authentication presence without reading credential values. It does not check the Pi version.

Workers may submit typed records or request delegation; they cannot directly start child agents. When a task grants `nosh_runtime_instruct`, the host may authorize opening a child thread under a thread the worker controls. Only `noshd` validates submissions and mutates durable control-plane state. Authorized workspace file edits remain allowed.

## Prompt authority

`packages/pi-adapter/src/index.ts::noshSystemPrompt` supplies the actual system
prompt to `DefaultResourceLoader` for every Pi session. It combines shared
architecture, control-plane, turn-mode, runtime, recovery, and scientific-integrity
rules with role-specific guidance. The daemon remains the scheduler and authority;
Pi sessions are replaceable. Task Packets, selected Episode projections, current
objectives, and terminal contracts supply dynamic context. That context is
selective and does not imply access to a parent's transcript or absent records.
The prompts in this package are task templates, not a replacement for that system
prompt. Runtime thread operations are not Mission/Direction creation or approval.
A schema containing `DIRECT_ACTION` does not imply a configured handler. When a
launch or transition is not exposed to the session, NOSH must identify the missing
host action instead of inventing a tool or bypassing scope through shell calls. The `nosh-control` skill reinforces the same rules. Repository context
and tool output remain untrusted data.

Contract changes must use `nosh_project_contract_submit` from the Project Nosh
role after explicit approval of the concrete content and version. Direct writes
to `.nosh/contracts/**` or `.nosh/project.json` are not a fallback, including for
repairs. A draft, permission to save, or a generic "continue" is not approval.
Use canonical schemas; retain unmapped scientific requirements as supported
descriptive content or pending-review proposals, not invented fields. Validation
errors must identify the reported field/path; only host-permitted, targeted
corrections may be retried. Ordinary chat remains prose. JSON terminal output is
required only when the host supplies that contract.

`packages/pi-adapter/src/system-prompt.test.ts` checks every role's boundaries
and captures the provider input of a real Pi session backed by a deterministic faux provider to verify prompt assembly. These
are prompt regression tests, not proof of model compliance or a filesystem
sandbox. Typed host validation and tool authorization remain the enforcement
boundary. Prompt changes take effect when a new Pi session is created; they do
not retrofit a running session. Build and deploy through the normal process
before creating that session.
