# Terminal workspace

Run `nosh open` or `nosh tui` in an interactive terminal with Bun >=1.3. Node runs the daemon; Bun runs the OpenTUI client. Configure provider credentials through Pi outside the TUI.

## OpenCode-style workspace

The home screen shows the NOSH mark (a gradient block wordmark), the composer, colored key hints, and the open project. Every conversation also opens with the mark, above its first message. The sidebar heading carries a compact wordmark.

The home screen has a centered NOSH logo (a one-line wordmark below 20 rows) and a multiline composer. Conversations use Markdown, separate user/assistant/tool blocks, and a research sidebar. The sidebar is hidden on the home screen and below 80 columns. It shows one line per record with section counts and a **Needs you** section for a staged action and pending graph proposals. The footer shows the repository path, a connection badge (`● connected`, `○ connecting`, `○ reconnecting`), and `N O S H 0.1.0`. Inspector views show a scrollbar only when content overflows; the status line animates while work is in progress. Upstream source and MIT notices are in [UPSTREAM.md](../../apps/tui/UPSTREAM.md).

Tool calls are titled by their target (for example `bash ls .nosh` or `read docs/paper.md`) and show at most six lines or 360 characters of output, followed by `… N more lines`. The composer label shows the active agent role and model, loaded from the daemon when an agent starts.

`/help` shows the authoritative key and command reference.

- **Enter:** send. **Shift+Enter** or **Alt+Enter:** insert a newline. The composer holds up to 16,000 characters.
- **Ctrl+P:** searchable command palette with shortcuts shown beside entries. Typing `/` opens slash-command search.
- **Ctrl+O:** switch project, with open-existing and create-new forms.
- **F2:** select an authenticated model, then one of its advertised thinking levels. Ctrl+M is not used because most terminals send it as Enter.
- **Ctrl+T:** cycle to the selected model's next advertised thinking level; with no model selected, it opens the picker.
- **Ctrl+B:** toggle the research sidebar.
- **Ctrl+G** (or `/map`): the research map. It is a live tree of what the Project is doing: each Mission and Direction with a progress bar and its graph nodes (with the worker holding each node, its current tool and model), each Autoresearch with its round and best score and every experiment's state (proposed, implementing, evaluating, ★ promoted, held, rejected, failed) and score delta, all live workers, and running Jobs. Glyphs and colors: ✓ done, ● running, ◐ waiting or in review, ✗ failed or blocked, ○ not started. It refreshes every few seconds while open; Esc returns to the conversation.
- **Esc:** close a dialog without applying an action; from any other view, return to the conversation. In the conversation a single Esc keeps the draft; a second Esc within 1.5 seconds clears it (the status line shows `esc again to clear the draft`).
- **Up/Down** at composer boundaries recall input history (100 entries, this session only).
- **PgUp/PgDn:** scroll. **Ctrl+Home/End:** jump to start/end.
- **Ctrl+C** or `/quit`: detach. The daemon and Jobs continue running.
- **Mouse:** click the model or thinking label to open its picker, the footer path to switch project, or a dialog row to choose it.

In dialogs, Up/Down or Tab/Shift+Tab move, PgUp/PgDn move by eight, Enter chooses, and Space toggles the create-repository option in the project form.

Without a project, the composer reads `Choose a project first — press ctrl+o`, and sending a message opens the project picker. Pick a model your Pi account lists (for example `openai-codex/gpt-5.6-luna` with `medium` or `low`, if available). Raw commands remain available: dismiss slash search with Esc to enter arguments directly, or choose an argument-taking command in the palette.

## Commands

- `/projects`, `/project <id>`: list/select registered Projects.
- `/open` or `/new` with no arguments: open the open-existing or create-repository form. `/open {"path":"/repo","workingTitle":"Study","createRepository":false}` opens a Project directly; invalid JSON shows this usage. Opening begins intake. CLI `nosh project open <path>` only registers it without model intake.
- `/models`, `/model <provider> <id> [thinking]`, `/model default`, `/thinking`: select a model and thinking level. Without `[thinking]`, `off` is used if advertised, otherwise the first advertised level. Switching projects resets selection; opening a project preserves the explicitly selected intake model.
- `/status [missions|directions|autoresearch|agents]`, `/jobs`, `/job <id>`, `/tail <id> [stdout|stderr]`: inspect state. Cards show each entity's `id` and version so `/transition`, `/control`, and `/approve` arguments can be copied, with up to eight fields per card (`+N more fields`). Empty views explain what will appear instead of showing raw JSON.
- `/approvals`: inspect proposals and contract. An unapproved contract is shown as `draft`; it becomes `approved` only after explicit approval in the intake conversation. `/approve <proposalId> <version>` stages approval of the inspected version.
- `/start <missions|directions|autoresearch> <id> <version>` (approve and start a draft in one confirmation), `/export-paper` (render `docs/paper.md` and `docs/paper.bib` into `.nosh/paper-export`), `/cancel <id>`, `/checkpoint <id>`, `/transition <missions|directions|autoresearch> <id> <version> <state>` (to stop or pause a Mission use `/control`, which also cancels its Jobs and ends its sessions), `/retry <missions|directions> <id> <version> <nodeId>` (return a blocked or failed node to `ready`), `/steer <missionId> <version> <message>` (bounded Mission Director steer), `/control <missionId> <version> <pause|resume|stop> [safe|checkpoint|immediate]`, `/create <missions|directions|autoresearch> <JSON file path>`, `/amend-contract <JSON file path>` (write the next approved Project contract version, e.g. to declare `execution.commands` for `nosh_run`) (at most 256 KB): stage actions.
- `/confirm`: review/apply the staged action; `/discard`: clear it. The confirmation dialog defaults to keeping the action staged, not applying it. Chat and Project intake are immediate, not staged.
- `/paths`, `/chat`, `/refresh`, `/help`: paths (including the active `.nosh/contracts/project.v<N>.json` pattern), conversation, reload, and reference.

## Troubleshooting

If an older build floods the terminal with `Anchor is the same as the node` warnings, close the TUI, update/rebuild NOSH, and reopen it. Changing Windows Terminal settings does not fix transcript reconciliation. From a source checkout, run `corepack pnpm build`; `bun apps/tui/dist/smoke.js` checks native rendering, including populated conversations.

A `400` validation error is separate from rendering. Errors now show the failing fields instead of raw multiline schema JSON. If the error names invalid Project metadata or an invalid active contract, inspect the exact file reported. The error states whether the file is missing, is not valid JSON, or could not be read, and does not echo its content. `activeProjectContractPath` in `.nosh/project.json` must name `.nosh/contracts/project.v<N>.json`. Back up the file before repair and validate it against `packages/wire/src/schemas/project-contract.v1.schema.json` (metadata: `project-root.v1.schema.json`). Do not delete unknown research constraints, switch to an older contract, or mark changed content approved just to bypass validation. Keep the original and review a schema-valid replacement through the Project approval workflow. Until repaired, other Projects remain selectable but chat for the invalid Project is blocked before new message or agent side effects.

After updating daemon code, restart it when active work can safely be interrupted: `nosh stop`, then `nosh tui`. Closing only the TUI leaves the existing daemon running.

## State and limits

The daemon owns versions, permissions, receipts, and state. Mutations are not retried after ambiguous network failures. Inspect durable state before retrying. A staged action is bound to its project, inspected version, and idempotency key.

Live WebSocket text/tool updates are transient. A bounded HTTP bootstrap and periodic reconciliation retain up to 400 durable events; the visible transcript keeps the last 120 entries and 64,000 characters, each item clipped to 16,000. Chat and project-open requests time out after 120 seconds on the client; other requests after 15 seconds. A timeout does not prove the daemon did not act. Disconnects clear incomplete drafts and reconnect from the durable cursor. This is not a complete audit-log viewer.

No browser, relay, embedded shell, graph canvas, paper editor, artifact viewer, or foreground-fork takeover is restored. Use external editors/terminals and the local typed API for other operations. The OpenCode appearance does not import its coding-agent backend or all of its plugins/features.
