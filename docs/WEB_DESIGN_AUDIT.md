# Adversarial audit against the NOSH Web App Design Specification

Audit date: 2026-07-18 EDT  
Normative source: user-supplied `NOSH Web App Design Specification`, version 1.0  
Engineering authority: `C:\NOSH\NOSH_Research_Product_Engineering_Specification.md`

## Verdict

The functional version-1 web baseline is implemented. The application has the required Flexoki shell, Normal and Mission workspaces, typed daemon controls, responsive remote surface, local PTY terminal, and Project setup. It builds and its daemon boundaries are integration-tested.

It is **not yet a production design-acceptance pass**. Formal assistive-technology evidence, high-volume UI fixtures, physical mobile/remote evidence, a current post-PTY screenshot matrix, and the 48-hour soak do not exist. Those are release gates, not results that can be inferred from TypeScript compilation.

## Implemented baseline

| Area | Evidence in this tree |
|---|---|
| Shell | Independently persistent/resizable Project sidebar, central workspace, research inspector, and bottom workbench; compact drawers and phone navigation. |
| Visual system | Centralized dark Flexoki tokens, scarce purple selection/focus, semantic status colors with text, self-hosted Geist/Geist Mono, square border-led geometry, no gradients/glass/decorative animation. |
| Normal mode | Pi-backed chat, live typed events, bounded Direction and Autoresearch launch sheets, structured model override, context meter, explicit Normal/Mission control under the input. |
| Mission mode | Board, DAG, Timeline, Completion, Mission Chat, typed steering, explicit planning/approval/start/pause/resume/stop, display-only card drag, compact tabs, and wide Board/Chat split. |
| Research inspection | Mission/node state, agents, Directions, runs/jobs, Reviews, model/context/current operation, elapsed state, steering and cancellation. |
| Graph accessibility | Mission, experiment, and execution-thread canvases each have a textual dependency table using the same records. |
| Workbench | Terminal, Logs, Problems, and Jobs; real daemon-owned PowerShell/WSL PTYs with tabs, bounded replay, Project-root cwd, input/resize, explicit close, and Flexoki xterm rendering. |
| Remote boundary | Remote clients get encrypted semantic controls, logs, Jobs, and foreground-fork interaction; the daemon publishes no remote shell route. |
| Project setup | One GUI sheet initializes or opens a Git root through the same initializer as the CLI, then writes the Project contract, schema lock, paper workspace, and operational registration. |
| Authority | Authoritative actions wait for `noshd`; graph/card selection and local layout remain presentation state. Mission switching performs no lifecycle action. |

## Adversarial findings

### Release-blocking evidence gaps

1. No axe report, NVDA run, VoiceOver run, 200% zoom record, or complete keyboard-only acceptance record has been retained.
2. The browser capture matrix covered Normal, compact/wide Mission, and phone layouts before the final PTY replacement. A fresh 390/768/1100/1440/1920 capture and console pass is still required.
3. The 100,000-row, 10 MB scrollback, 500/2,000-node graph, 1,000-experiment, and 25,000-event reconnect fixtures have not been run. Client event projections are bounded, but this is not equivalent to the prescribed performance evidence.
4. No physical iPhone/Mac remote run proves pairing, reconnect, revoke/rekey, semantic Mission control, and remote-shell absence in the final UI.
5. The required 48-hour open-Mission soak has not run.

### Implemented-surface limitations

- Terminal tabs and both requested shell profiles work, but the optional one-level terminal split interaction is not implemented.
- The inspector exposes the critical live state and controls, but pin/history and the complete Overview/Live/Changes/Jobs/Handoff/Controls sub-tab treatment remain compact rather than exhaustive.
- Project creation/search/navigation are implemented; the row overflow operations and separate archive group are not all surfaced in the sidebar.
- Conversation/thread history is available in the dedicated Threads surface, not yet through the exact top-breadcrumb conversation menu described by the design document.
- Mission Completion renders deterministic criterion status, but the full evidence/claim/limitation/policy-exception matrix remains distributed across Completion, Evidence, Reviews, and Problems.
- Long feeds are deliberately bounded in the current projection rather than backed by the specified virtualized 100,000-row fixture. This is acceptable for the present build but does not satisfy the scale acceptance gate.
- The production build contains two large lazy chunks (about 954 kB and 1.13 MB uncompressed). They are not initial-route proof of failure, but low-end mobile p95 and cache behavior remain unmeasured.

These limitations are recorded rather than disguised with placeholder controls. None grants browser-side authority or weakens the remote-shell boundary.

## Verification performed

- `CI=true corepack pnpm release:check`: frozen install, TypeScript project build, 97 tests, production PWA, notices, and CycloneDX SBOM passed.
- The daemon integration suite creates a complete Git-backed Project through `/projects/open` and proves an authenticated PowerShell PTY/WebSocket input-output-close cycle.
- `corepack pnpm schemas`: 50 JSON Schema documents and 250 fixtures generated and verified; only the documented recursive `JsonValue` exporter warnings occurred.
- `corepack pnpm package:windows`: Windows distribution generated.
- `corepack pnpm soak -- --seconds=10`: 10 events, 4,096-byte database, and negative RSS growth. This is smoke evidence only.
- `git diff --check`: no whitespace errors; only Windows line-ending notices.

## Required operator gates

Run the exact checklist in [`docs/testing/release-gates.md`](testing/release-gates.md). Do not mark this design accepted until all manual/external evidence above is retained against a clean reviewed commit.
