import { createId } from "@nosh/core";
import { spawn, type IPty } from "node-pty";
import { join } from "node:path";

export type TerminalInfo = { terminalId: string; projectId: string; cwd: string; profile: "powershell" | "wsl"; startedAt: string; exitedAt: string | null; exitCode: number | null };
type Session = { info: TerminalInfo; process: IPty; buffer: string; listeners: Set<(message: object) => void> };

export class TerminalSessions {
  private readonly sessions = new Map<string, Session>();

  create(projectId: string, cwd: string, profile: "powershell" | "wsl" = "powershell"): TerminalInfo {
    const terminalId = createId("trm");
    const command = process.platform === "win32" ? profile === "wsl" ? "wsl.exe" : join(process.env.SystemRoot ?? "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe") : process.env.SHELL ?? "/bin/sh";
    const child = spawn(command, [], { name: "xterm-256color", cols: 120, rows: 30, cwd, env: process.env, useConptyDll: process.platform === "win32" });
    const info: TerminalInfo = { terminalId, projectId, cwd, profile, startedAt: new Date().toISOString(), exitedAt: null, exitCode: null };
    const session: Session = { info, process: child, buffer: "", listeners: new Set() };
    child.onData((data) => { session.buffer = `${session.buffer}${data}`.slice(-1_000_000); for (const listener of session.listeners) listener({ type: "output", data }); });
    child.onExit(({ exitCode }) => { session.info = { ...session.info, exitedAt: new Date().toISOString(), exitCode }; for (const listener of session.listeners) listener({ type: "exit", exitCode }); });
    this.sessions.set(terminalId, session);
    return info;
  }

  attach(terminalId: string, listener: (message: object) => void): () => void {
    const session = this.required(terminalId); session.listeners.add(listener); if (session.buffer) listener({ type: "output", data: session.buffer }); if (session.info.exitedAt) listener({ type: "exit", exitCode: session.info.exitCode }); return () => session.listeners.delete(listener);
  }

  write(terminalId: string, data: string): void { if (data.length > 64_000) throw new Error("terminal input exceeds 64 KB"); this.required(terminalId).process.write(data); }
  resize(terminalId: string, cols: number, rows: number): void { if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 2 || rows < 1 || cols > 500 || rows > 300) throw new Error("invalid terminal dimensions"); this.required(terminalId).process.resize(cols, rows); }
  close(terminalId: string): void { const session = this.required(terminalId); if (!session.info.exitedAt) session.process.kill(); this.sessions.delete(terminalId); }
  closeAll(): void { for (const session of this.sessions.values()) if (!session.info.exitedAt) session.process.kill(); this.sessions.clear(); }
  get(terminalId: string): TerminalInfo { return this.required(terminalId).info; }

  private required(terminalId: string): Session { const session = this.sessions.get(terminalId); if (!session) throw new Error("Terminal session not found"); return session; }
}
