# Run NOSH

Requirements: Node >=22.19.0, Bun >=1.3, Git, and a model account (a ChatGPT or Claude subscription, or an API key); `nosh setup` connects it. Use Node for the daemon and CLI; Bun is launched automatically for the terminal UI.

## Windows PowerShell

```powershell
cd C:\path\to\NOSH
node --version
bun --version
git --version
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js open
```

`setup` checks the prerequisites (with a fix for each failure), connects a model account if none is connected, starts the daemon, and asks for the default model; see [Installation](installation.md). If Corepack is unavailable, install pnpm 10.28.0 and use `pnpm` instead of `corepack pnpm`. Install Bun from https://bun.sh if it is missing, then open a new terminal. Do not use Linux `node_modules` from native Windows: run installation in the environment you will use.

## WSL / Linux

```sh
cd /path/to/NOSH
node --version
bun --version
git --version
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js open
```

Keep build/install/launch in one environment. Pi credentials must be available in that same environment. A Windows Pi login is not automatically a WSL Pi login.

## Inside the TUI

- Ctrl+O: open/create or select a Project. Opening starts discovery: the intake agent asks one question at a time, then proposes the Project contract for your approval.
  - It also proposes the repository's test and evaluation commands (detected from `package.json`, pytest, or a `Makefile`) so workers can run them as daemon Jobs.
  - On approval, NOSH commits exactly the two contract files, so the checkout stays clean for research tasks.
  - Later changes go through `/amend-contract <file>`, which is committed the same way.
- F2: choose a model your Pi account lists (for example `openai-codex/gpt-5.6-luna`, if available), then a thinking level it advertises. Ctrl+T cycles thinking levels.
- Default model: sessions that select no model, including all Director, worker, reviewer, and librarian sessions, use `defaultModel` from `%LOCALAPPDATA%\NOSH\config.json`, written as `provider/id` or `provider/id:thinkingLevel` (for example `"defaultModel": "openai-codex/gpt-6-luna:low"`). Restart the daemon (`nosh stop`, then `nosh start`) to apply it. Without it, Pi's global `~/.pi/agent/settings.json` default applies, which other Pi clients can change.
- Ctrl+P: searchable commands. Enter sends; Shift+Enter adds a newline.
- `/help` lists every key and command.
- `/quit` or Ctrl+C detaches. It does not stop the daemon or Jobs.

Start again with `node apps/cli/dist/main.js tui`. To stop the daemon explicitly:

```sh
node apps/cli/dist/main.js stop
```

If launch fails, use `node apps/cli/dist/main.js status`, `doctor`, or `logs`. Cold starts may take up to 90 seconds before the launcher reports a timeout. Do not paste authentication tokens into commands.
