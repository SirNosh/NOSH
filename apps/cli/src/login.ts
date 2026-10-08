/**
 * Interactive model-provider sign-in for `nosh setup` and `nosh login`. Pi's runtime runs each provider's
 * own flow and stores the credential; this module only renders prompts. Secrets are read masked and never echoed.
 */
import { spawn } from "node:child_process";
import { createInterface } from "node:readline/promises";
import type { AuthInteraction, AuthProvider } from "@nosh/pi-adapter";

async function adapter() { return import("@nosh/pi-adapter"); }

const FEATURED_KEYS = ["openai", "anthropic", "google", "openrouter", "deepseek", "groq", "mistral", "xai"];

/** Providers with a stored credential, by name. */
export async function connectedProviders(): Promise<AuthProvider[]> {
  return (await (await adapter()).authProviders()).filter((provider) => provider.connected);
}

/** Lets the user bring a subscription or an API key; returns true when a provider was connected. */
export async function connectModelProvider(): Promise<boolean> {
  const providers = await (await adapter()).authProviders();
  const byId = new Map(providers.map((provider) => [provider.id, provider]));
  const choices: Array<{ label: string; detail: string; run: () => Promise<boolean> }> = [];
  const subscription = (id: string, label: string, detail: string) => { if (byId.get(id)?.subscription) choices.push({ label, detail, run: () => login(byId.get(id)!, "oauth") }); };
  subscription("openai-codex", "ChatGPT subscription (Plus/Pro)", "sign in with your OpenAI account");
  subscription("anthropic", "Claude subscription (Pro/Max)", "sign in with your Anthropic account");
  choices.push({ label: "API key", detail: "OpenAI, Anthropic, Gemini, OpenRouter, DeepSeek, …", run: () => apiKey(providers) });
  const others = providers.filter((provider) => provider.subscription && !["openai-codex", "anthropic"].includes(provider.id));
  if (others.length) choices.push({ label: "Other sign-in", detail: others.map((provider) => provider.name).join(", "), run: async () => { const picked = await choose("Sign in with", others.map((provider) => ({ label: provider.subscription!, detail: provider.connected ? "connected" : "" }))); return picked === null ? false : login(others[picked]!, "oauth"); } });
  choices.push({ label: "Skip for now", detail: "run `nosh login` later", run: async () => false });
  // An empty or mistyped answer asks again; skipping is an explicit choice, never an accident.
  let picked = await choose("How should NOSH reach a model?", choices);
  while (picked === null) { process.stdout.write("Enter one of the numbers above (pick “Skip for now” to continue without a model).\n"); picked = await choose("How should NOSH reach a model?", choices); }
  return choices[picked]!.run();
}

async function apiKey(providers: AuthProvider[]): Promise<boolean> {
  const keyed = providers.filter((provider) => provider.apiKey);
  const featured = FEATURED_KEYS.map((id) => keyed.find((provider) => provider.id === id)).filter((provider): provider is AuthProvider => Boolean(provider));
  const rest = keyed.filter((provider) => !featured.includes(provider)).sort((left, right) => left.name.localeCompare(right.name));
  // The common providers fit on one screen; the long tail is one choice away.
  const shown = rest.length ? [...featured.map((provider) => ({ label: provider.name, detail: provider.connected ? "connected" : "", provider })), { label: "More providers…", detail: `${rest.length} more`, provider: null }] : featured.map((provider) => ({ label: provider.name, detail: provider.connected ? "connected" : "", provider }));
  const picked = await choose("Which provider is the key for?", shown);
  if (picked === null) return false;
  const chosen = shown[picked]!.provider;
  if (chosen) return login(chosen, "api_key");
  const more = await choose("Which provider is the key for?", rest.map((provider) => ({ label: provider.name, detail: provider.connected ? "connected" : "" })));
  return more === null ? false : login(rest[more]!, "api_key");
}

async function login(provider: AuthProvider, type: "oauth" | "api_key"): Promise<boolean> {
  process.stdout.write(`\n${type === "oauth" ? `Signing in: ${provider.subscription}` : `Adding ${provider.apiKey}`}\n`);
  try {
    await (await adapter()).loginProvider(provider.id, type, interaction());
    process.stdout.write(`✓ ${provider.name} connected\n`);
    return true;
  } catch (error) {
    process.stdout.write(`✗ ${provider.name} was not connected: ${error instanceof Error ? error.message.split("\n")[0] : "sign-in failed"}\n`);
    return false;
  }
}

/** Renders Pi's sign-in prompts and events in the terminal. */
function interaction(): AuthInteraction {
  return {
    async prompt(request) {
      // A prompt the flow cancels (the browser sign-in won the race) must release the terminal at once.
      if (request.type === "secret") return secret(`${request.message}${request.placeholder ? ` (${request.placeholder})` : ""}: `, request.signal);
      if (request.type === "select") { const picked = await choose(request.message, request.options.map((option) => ({ label: option.label, detail: option.description ?? "" }))); if (picked === null) throw new Error("cancelled"); return request.options[picked]!.id; }
      return ask(`${request.message}${request.placeholder ? ` (${request.placeholder})` : ""}: `, request.signal);
    },
    notify(event) {
      if (event.type === "auth_url") { process.stdout.write(`\nOpening your browser to sign in. If it does not open, visit:\n  ${event.url}\n${event.instructions ? `${event.instructions}\n` : ""}`); openBrowser(event.url); }
      else if (event.type === "device_code") process.stdout.write(`\nVisit ${event.verificationUri} and enter the code: ${event.userCode}\n`);
      else process.stdout.write(`${event.message}\n`);
    },
  };
}

async function ask(question: string, signal?: AbortSignal): Promise<string> {
  const prompt = createInterface({ input: process.stdin, output: process.stdout });
  const abort = () => { process.stdout.write("\n"); prompt.close(); };
  signal?.addEventListener("abort", abort, { once: true });
  try { return (await prompt.question(question, signal ? { signal } : {})).trim(); }
  finally { signal?.removeEventListener("abort", abort); prompt.close(); }
}

/** Numbered menu; returns the chosen index, or null for an empty or invalid answer. */
async function choose(title: string, options: Array<{ label: string; detail: string }>): Promise<number | null> {
  process.stdout.write(`\n${title}\n`);
  const width = Math.max(...options.map((option) => option.label.length));
  options.forEach((option, index) => process.stdout.write(`  ${String(index + 1).padStart(2)}. ${option.label.padEnd(width)}  ${option.detail}\n`));
  // A mistyped number is asked again; only an empty answer cancels.
  for (;;) {
    const raw = await ask(`Choose 1-${options.length}: `); if (!raw) return null;
    const answer = Number(raw);
    if (Number.isInteger(answer) && answer >= 1 && answer <= options.length) return answer - 1;
    process.stdout.write(`“${raw}” is not one of 1-${options.length}.\n`);
  }
}

/** Reads a secret without echoing it (one `*` per character). */
function secret(question: string, signal?: AbortSignal): Promise<string> {
  if (!process.stdin.isTTY) return ask(question, signal);
  return new Promise((resolve, reject) => {
    process.stdout.write(question); let value = "";
    const stdin = process.stdin; stdin.setRawMode(true); stdin.resume(); stdin.setEncoding("utf8");
    const onAbort = () => done(new Error("cancelled"));
    const done = (error?: Error) => { signal?.removeEventListener("abort", onAbort); stdin.setRawMode(false); stdin.pause(); stdin.off("data", onData); process.stdout.write("\n"); if (error) reject(error); else resolve(value.trim()); };
    signal?.addEventListener("abort", onAbort, { once: true });
    const onData = (chunk: string) => {
      for (const char of chunk) {
        if (char === "\r" || char === "\n") return done();
        if (char === "\u0003") return done(new Error("cancelled"));
        if (char === "\u007f" || char === "\b") { if (value) { value = value.slice(0, -1); process.stdout.write("\b \b"); } continue; }
        if (char >= " ") { value += char; process.stdout.write("*"); }
      }
    };
    stdin.on("data", onData);
  });
}

function openBrowser(url: string): void {
  const [command, args] = process.platform === "win32" ? ["rundll32", ["url.dll,FileProtocolHandler", url]] : process.platform === "darwin" ? ["open", [url]] : ["xdg-open", [url]];
  try { spawn(command, args, { detached: true, stdio: "ignore", windowsHide: true }).on("error", () => undefined).unref(); } catch { /* the printed URL is the fallback */ }
}
