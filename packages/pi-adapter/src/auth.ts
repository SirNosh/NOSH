/**
 * Model-provider sign-in through Pi's own runtime: subscriptions (OAuth) and API keys are stored
 * where Pi stores them (shared with the `pi` CLI). NOSH never reads or prints credential values.
 */
import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import type { AuthInteraction, AuthType } from "@earendil-works/pi-ai";

export type { AuthInteraction } from "@earendil-works/pi-ai";
export type AuthProvider = {
  id: string;
  name: string;
  /** Subscription sign-in name (OAuth), e.g. "OpenAI (ChatGPT Plus/Pro)"; null when unsupported. */
  subscription: string | null;
  /** API-key name, e.g. "OpenAI API key"; null when the provider has no interactive key entry. */
  apiKey: string | null;
  connected: boolean;
};

type ProviderAuth = { oauth?: { name?: string }; apiKey?: { name?: string; login?: unknown } };

export async function authProviders(): Promise<AuthProvider[]> {
  const runtime = await ModelRuntime.create();
  return runtime.getProviders().map((provider) => {
    const auth = ((provider as { auth?: ProviderAuth }).auth ?? {});
    return { id: provider.id, name: provider.name ?? provider.id, subscription: auth.oauth ? auth.oauth.name ?? provider.name ?? provider.id : null, apiKey: auth.apiKey?.login ? auth.apiKey.name ?? `${provider.name ?? provider.id} API key` : null, connected: runtime.hasConfiguredAuth(provider.id) };
  });
}

/** Runs the provider's own sign-in flow; prompts and browser/device-code events go through `interaction`. */
export async function loginProvider(providerId: string, type: AuthType, interaction: AuthInteraction): Promise<void> {
  const runtime = await ModelRuntime.create();
  await runtime.login(providerId, type, interaction);
}

export async function logoutProvider(providerId: string): Promise<void> {
  const runtime = await ModelRuntime.create();
  await runtime.logout(providerId);
}
