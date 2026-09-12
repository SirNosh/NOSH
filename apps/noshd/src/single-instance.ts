import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";

export class SingleInstanceLock {
  private readonly token = randomUUID();
  private acquired = false;

  constructor(private readonly path: string) {}

  acquire(): void {
    try {
      writeFileSync(this.path, JSON.stringify({ pid: process.pid, token: this.token }), { encoding: "utf8", flag: "wx", mode: 0o600 });
      this.acquired = true;
      return;
    } catch (error: unknown) {
      if (!(error instanceof Error) || !("code" in error) || error.code !== "EEXIST") throw error;
    }

    if (!this.removeStaleLock()) throw new Error(`Another noshd instance owns ${this.path}`);
    writeFileSync(this.path, JSON.stringify({ pid: process.pid, token: this.token }), { encoding: "utf8", flag: "wx", mode: 0o600 });
    this.acquired = true;
  }

  release(): void {
    if (!this.acquired || !existsSync(this.path)) return;
    const current = readFileSync(this.path, "utf8");
    if (current.includes(this.token)) rmSync(this.path);
    this.acquired = false;
  }

  private removeStaleLock(): boolean {
    try {
      const { pid } = JSON.parse(readFileSync(this.path, "utf8")) as { pid?: number };
      if (typeof pid === "number" && pid > 0) {
        try {
          process.kill(pid, 0);
          return false;
        } catch (error: unknown) {
          if (error instanceof Error && "code" in error && error.code === "EPERM") return false;
        }
      }
      rmSync(this.path);
      return true;
    } catch {
      return false;
    }
  }
}
