# Run NOSH

Requirements: Node >=22.19.0, Bun >=1.3, Git, and provider authentication configured in Pi. Use Node for the daemon and CLI; Bun is launched automatically for the terminal UI.

## Windows PowerShell

```powershell
cd C:\NOSH\NOSH
node --version
bun --version
git --version
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js tui
```

If Corepack is unavailable, install pnpm 10.28.0 and use `pnpm` instead of `corepack pnpm`. Install Bun from https://bun.sh if it is missing, then open a new terminal. Do not use Linux `node_modules` from native Windows: run installation in the environment you will use.

## WSL / Linux

```sh
cd /mnt/c/NOSH/NOSH
node --version
bun --version
git --version
corepack pnpm install --frozen-lockfile
corepack pnpm build
node apps/cli/dist/main.js setup
node apps/cli/dist/main.js tui
```

Keep build/install/launch in one environment. Pi credentials must be available in that same environment. A Windows Pi login is not automatically a WSL Pi login.

## Inside the TUI

- Ctrl+O: open/create or select a Project. Opening starts intake; choose your model first if you want a specific one.
- Ctrl+M: choose `openai-codex/gpt-5.6-luna`, then `medium` or `low` when your account lists it.
- Ctrl+P: searchable commands. Enter sends; Shift+Enter adds a newline.
- `/quit` detaches. It does not stop the daemon or Jobs.

Start again with `node apps/cli/dist/main.js tui`. To stop the daemon explicitly:

```sh
node apps/cli/dist/main.js stop
```

If launch fails, use `node apps/cli/dist/main.js status`, `doctor`, or `logs`. Cold starts may take up to 90 seconds before the launcher reports a timeout. Do not paste authentication tokens into commands.
