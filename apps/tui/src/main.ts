#!/usr/bin/env bun
import { launchTui } from './index.js';
import type { TuiConfig } from './client.js';
const encoded = process.env.NOSH_TUI_CONFIG;
delete process.env.NOSH_TUI_CONFIG;
try {
  const config: TuiConfig = encoded ? JSON.parse(encoded) : {
    baseUrl: process.env.NOSH_BASE_URL ?? 'http://127.0.0.1:4321',
    ...(process.env.NOSH_BOOTSTRAP_TOKEN ? { bootstrapToken: process.env.NOSH_BOOTSTRAP_TOKEN } : {}),
    ...(process.env.NOSH_SESSION_TOKEN ? { sessionToken: process.env.NOSH_SESSION_TOKEN } : {}),
    ...(process.env.NOSH_PROJECT_ID ? { currentProjectId: process.env.NOSH_PROJECT_ID } : {}),
  };
  delete process.env.NOSH_BOOTSTRAP_TOKEN;
  delete process.env.NOSH_SESSION_TOKEN;
  await launchTui(config);
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
