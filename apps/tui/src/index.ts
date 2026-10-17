import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import type { TuiConfig } from './client.js';
export type { TuiConfig } from './client.js';
/** Safe to import in the Node 22 daemon CLI: native OpenTUI is loaded only in Bun. */
export async function launchTui(config: TuiConfig): Promise<void> {
  if ('Bun' in globalThis) {
    const { runTui } = await import('./ui.js');
    return runTui(config);
  }
  await new Promise<void>((resolve, reject) => {
    const child = spawn(process.env.NOSH_BUN ?? 'bun', [fileURLToPath(new URL('./main.js', import.meta.url))], {
      stdio: 'inherit', env: { ...process.env, NOSH_TUI_CONFIG: JSON.stringify(config) },
    });
    child.once('error', error => reject(new Error(`OpenTUI requires Bun >=1.3. Install Bun or set NOSH_BUN. ${error.message}`)));
    child.once('exit', (code, signal) => code === 0 || signal === 'SIGINT' ? resolve() : reject(new Error(`TUI exited (${code ?? signal})`)));
  });
}
