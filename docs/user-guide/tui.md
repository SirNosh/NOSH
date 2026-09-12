# Terminal workspace

Run `nosh open` or `nosh tui` in an interactive terminal with Bun >=1.3. Node runs the daemon; Bun runs the OpenTUI client. Configure provider credentials through Pi outside the TUI.

## OpenCode-style workspace

The home screen has a centered NOSH logo and multiline composer. Conversations use Markdown, separate user/assistant/tool blocks, and a research sidebar. The sidebar hides below 80 columns. Upstream source and MIT notices are in [UPSTREAM.md](../../apps/tui/UPSTREAM.md).

- **Enter:** send. **Shift+Enter:** insert a newline.
- **Ctrl+P:** searchable command palette. Typing `/` opens slash-command search.
- **Ctrl+O:** project picker, with existing/new project forms.
- **Ctrl+M:** authenticated model picker, then supported thinking level.
- **Tab:** thinking-level picker. **Ctrl+B:** toggle research sidebar.
- **Esc:** close a dialog without applying an action; otherwise clear the composer or return to chat.
- **PgUp/PgDn:** scroll. **Ctrl+Home/End:** jump to start/end.
- **Up/Down** at composer boundaries recalls local input history.
- **Ctrl+C** or `/quit`: detach. The daemon and Jobs continue running.

Use the model picker to select `openai-codex/gpt-5.6-luna` with `medium` or `low`, if your Pi account exposes it. Raw commands remain available: dismiss slash search with Esc to enter arguments directly, or choose an argument-taking command in the palette.

## Command fallbacks

- `/projects`, `/project <id>`: list/select registered Projects.
- `/open {"path":"/repo","workingTitle":"Study","createRepository":false}`: open a Project and begin intake. CLI `nosh project open <path>` only registers it without model intake.
- `/models`, `/model <provider> <id> [thinking]`, `/model default`: select model. Switching projects resets selection; opening a project preserves the explicitly selected intake model.
- `/status [missions|directions|autoresearch|agents]`, `/jobs`, `/job <id>`, `/tail <id> [stdout|stderr]`: inspect state.
- `/approvals`: inspect proposals and contract. `/approve <proposalId> <version>` stages approval of the inspected version.
- `/cancel <id>`, `/checkpoint <id>`, `/transition <missions|directions|autoresearch> <id> <version> <state>`, `/control <missionId> <version> <pause|resume|stop> [safe|checkpoint|immediate]`, `/create <missions|directions|autoresearch> <JSON file path>`: stage actions.
- `/confirm`: review/apply the staged action; `/discard`: clear it. The confirmation dialog defaults to keeping the action staged, not applying it. Chat and Project intake are immediate, not staged.
- `/paths`, `/chat`, `/refresh`, `/help`: paths, conversation, reload, and reference.

## State and limits

The daemon owns versions, permissions, receipts, and state. Mutations are not retried after ambiguous network failures. Inspect durable state before retrying. A staged action is bound to its project, inspected version, and idempotency key.

Live WebSocket text/tool updates are transient. A bounded HTTP bootstrap and periodic reconciliation retain up to 400 durable events; the visible transcript is also bounded. Disconnects clear incomplete drafts and reconnect from the durable cursor. This is not a complete audit-log viewer.

No browser, relay, embedded shell, graph canvas, paper editor, artifact viewer, or foreground-fork takeover is restored. Use external editors/terminals and the local typed API for other operations. The OpenCode appearance does not import its coding-agent backend or all of its plugins/features.
