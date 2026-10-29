//! `apps/cli/src/login.ts`, and `loginCommand` and `logoutCommand` in `main.ts`. Pi runs each
//! provider's own sign-in flow in the sidecar and stores the credential; this only renders its
//! prompts. Secrets are read masked and never echoed.

use super::prompt::{self, RawMode};
use super::{Cli, js_trim, out, pad_end, pad_start};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nosh_agent::async_calls::BoxFuture;
use nosh_agent::pi::{AuthInteraction, AuthSignal, PiAdapter, PiConfig, node_executable};
use nosh_server::noshd::Installation;
use nosh_wire::js;
use serde_json::Value;
use std::cmp::Ordering;
use std::io::IsTerminal;
use std::sync::Arc;

const FEATURED_KEYS: [&str; 8] = [
    "openai",
    "anthropic",
    "google",
    "openrouter",
    "deepseek",
    "groq",
    "mistral",
    "xai",
];

/// What a top-menu choice of `connectModelProvider` runs.
enum Choice<'a> {
    Subscription(&'a Value),
    ApiKey,
    OtherSignIn,
    Skip,
}

impl Cli {
    /// `adapter()`: the Pi sidecar, started on first use. Its stderr is not shown; the terminal is
    /// the sign-in's.
    fn pi(&self) -> Result<&PiAdapter, String> {
        if let Some(pi) = self.pi.get() {
            return Ok(pi);
        }
        let installation = Installation::find()?;
        let config = PiConfig {
            node: node_executable(),
            host_script: installation.host_script,
            env: Vec::new(),
            schema_dir: installation.schema_dir,
            forward_stderr: false,
        };
        let pi = self.block_on(PiAdapter::spawn(config, Arc::new(|_| {}), None, None))?;
        Ok(self.pi.get_or_init(|| pi))
    }

    /// `authProviders()`: Pi's providers, in its order.
    fn auth_providers(&self) -> Result<Vec<Value>, String> {
        let providers = self.block_on(self.pi()?.auth_providers())?;
        Ok(providers.as_array().cloned().unwrap_or_default())
    }

    /// `connectedProviders()`: providers with a stored credential.
    pub(super) fn connected_providers(&self) -> Result<Vec<Value>, String> {
        let mut providers = self.auth_providers()?;
        providers.retain(|provider| js::truthy(provider.get("connected")));
        Ok(providers)
    }

    /// `loginCommand(argument)`: `--list` lists the providers; otherwise a sign-in, which needs a
    /// terminal.
    pub(super) fn login_command(&self) -> Result<(), String> {
        if self.flag("--list") {
            for provider in self.auth_providers()? {
                let kinds: Vec<&str> = [("subscription", "subscription"), ("apiKey", "api key")]
                    .into_iter()
                    .filter(|(key, _)| js::truthy(provider.get(key)))
                    .map(|(_, kind)| kind)
                    .collect();
                if kinds.is_empty() {
                    continue;
                }
                out(&format!(
                    "{} {} {}\n",
                    if js::truthy(provider.get("connected")) {
                        "●"
                    } else {
                        "○"
                    },
                    pad_end(&js::string(provider.get("id")), 24),
                    kinds.join(" + ")
                ));
            }
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            return Err("nosh login needs an interactive terminal".into());
        }
        if self.connect_model_provider()? {
            let config = if self.config_path.exists() {
                Some(self.load_config()?)
            } else {
                None
            };
            out(if config.is_some_and(|config| self.healthy(&config)) {
                "The running daemon picks it up on its next session; in the terminal UI, /refresh. Choose a default with nosh model set <provider/id:level>.\n"
            } else {
                "Next: nosh setup (or nosh model set <provider/id:level>)\n"
            });
        }
        Ok(())
    }

    /// `logoutCommand(providerId)`.
    pub(super) fn logout_command(&self, provider_id: &str) -> Result<(), String> {
        self.block_on(self.pi()?.logout(provider_id))?;
        out(&format!("Signed out of {provider_id}\n"));
        Ok(())
    }

    /// `connectModelProvider()`: a subscription or an API key; true when a provider was connected.
    pub(super) fn connect_model_provider(&self) -> Result<bool, String> {
        let providers = self.auth_providers()?;
        let mut choices: Vec<(String, String, Choice)> = Vec::new();
        for (id, label, detail) in [
            (
                "openai-codex",
                "ChatGPT subscription (Plus/Pro)",
                "sign in with your OpenAI account",
            ),
            (
                "anthropic",
                "Claude subscription (Pro/Max)",
                "sign in with your Anthropic account",
            ),
        ] {
            if let Some(provider) = providers
                .iter()
                .find(|provider| provider["id"] == id)
                .filter(|provider| js::truthy(provider.get("subscription")))
            {
                choices.push((label.into(), detail.into(), Choice::Subscription(provider)));
            }
        }
        choices.push((
            "API key".into(),
            "OpenAI, Anthropic, Gemini, OpenRouter, DeepSeek, …".into(),
            Choice::ApiKey,
        ));
        let others: Vec<&Value> = providers
            .iter()
            .filter(|provider| {
                js::truthy(provider.get("subscription"))
                    && provider["id"] != "openai-codex"
                    && provider["id"] != "anthropic"
            })
            .collect();
        if !others.is_empty() {
            let names: Vec<String> = others
                .iter()
                .map(|provider| js::string(provider.get("name")))
                .collect();
            choices.push((
                "Other sign-in".into(),
                names.join(", "),
                Choice::OtherSignIn,
            ));
        }
        choices.push((
            "Skip for now".into(),
            "run `nosh login` later".into(),
            Choice::Skip,
        ));
        let menu: Vec<(String, String)> = choices
            .iter()
            .map(|(label, detail, _)| (label.clone(), detail.clone()))
            .collect();
        // An empty or mistyped answer asks again; skipping is an explicit choice, never an accident.
        let title = "How should NOSH reach a model?";
        let picked = loop {
            if let Some(picked) = choose(title, &menu)? {
                break picked;
            }
            out(
                "Enter one of the numbers above (pick “Skip for now” to continue without a model).\n",
            );
        };
        Ok(match choices[picked].2 {
            Choice::Subscription(provider) => self.login(provider, "oauth"),
            Choice::ApiKey => self.api_key(&providers)?,
            Choice::OtherSignIn => {
                let menu: Vec<(String, String)> = others
                    .iter()
                    .map(|provider| {
                        (
                            js::string(provider.get("subscription")),
                            connected(provider),
                        )
                    })
                    .collect();
                choose("Sign in with", &menu)?
                    .is_some_and(|picked| self.login(others[picked], "oauth"))
            }
            Choice::Skip => false,
        })
    }

    /// `apiKey(providers)`: the common providers first; the long tail is one choice away.
    fn api_key(&self, providers: &[Value]) -> Result<bool, String> {
        let keyed: Vec<&Value> = providers
            .iter()
            .filter(|provider| js::truthy(provider.get("apiKey")))
            .collect();
        let featured: Vec<&Value> = FEATURED_KEYS
            .iter()
            .filter_map(|id| keyed.iter().copied().find(|provider| provider["id"] == *id))
            .collect();
        let mut rest: Vec<&Value> = keyed
            .iter()
            .copied()
            .filter(|provider| !featured.iter().any(|shown| std::ptr::eq(*shown, *provider)))
            .collect();
        rest.sort_by(|left, right| {
            locale_compare(
                &js::string(left.get("name")),
                &js::string(right.get("name")),
            )
        });
        let entry = |provider: &&Value| (js::string(provider.get("name")), connected(provider));
        let mut shown: Vec<(String, String)> = featured.iter().map(entry).collect();
        if !rest.is_empty() {
            shown.push(("More providers…".into(), format!("{} more", rest.len())));
        }
        let title = "Which provider is the key for?";
        let Some(picked) = choose(title, &shown)? else {
            return Ok(false);
        };
        if let Some(provider) = featured.get(picked) {
            return Ok(self.login(provider, "api_key"));
        }
        let more: Vec<(String, String)> = rest.iter().map(entry).collect();
        Ok(choose(title, &more)?.is_some_and(|picked| self.login(rest[picked], "api_key")))
    }

    /// `login(provider, type)`: the outcome is printed, never raised.
    fn login(&self, provider: &Value, auth_type: &str) -> bool {
        out(&if auth_type == "oauth" {
            format!(
                "\nSigning in: {}\n",
                js::string(provider.get("subscription"))
            )
        } else {
            format!("\nAdding {}\n", js::string(provider.get("apiKey")))
        });
        let name = js::string(provider.get("name"));
        let id = js::string(provider.get("id"));
        let signed_in = self
            .pi()
            .and_then(|pi| self.block_on(pi.login(&id, auth_type, Arc::new(Interaction))));
        match &signed_in {
            Ok(()) => out(&format!("✓ {name} connected\n")),
            Err(error) => out(&format!(
                "✗ {name} was not connected: {}\n",
                error.split('\n').next().unwrap_or_default()
            )),
        }
        signed_in.is_ok()
    }
}

/// `provider.connected ? "connected" : ""`.
fn connected(provider: &Value) -> String {
    if js::truthy(provider.get("connected")) {
        "connected".into()
    } else {
        String::new()
    }
}

/// `left.localeCompare(right)`, approximated: case-insensitive, lowercase first on a tie (ICU's
/// tertiary order). ICU's weighting of accents and punctuation is not reproduced.
fn locale_compare(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| right.cmp(left))
}

/// `interaction()`: renders Pi's sign-in prompts and events in the terminal.
struct Interaction;

impl AuthInteraction for Interaction {
    fn prompt(&self, prompt: Value, signal: AuthSignal) -> BoxFuture<Result<String, String>> {
        // The terminal read blocks; the sidecar's messages, `auth.cancel` among them, keep flowing.
        Box::pin(async move {
            tokio::task::spawn_blocking(move || answer(&prompt, &signal))
                .await
                .map_err(|error| error.to_string())?
        })
    }

    fn notify(&self, event: Value) {
        match event["type"].as_str() {
            Some("auth_url") => {
                let url = js::string(event.get("url"));
                let instructions = event.get("instructions");
                out(&format!(
                    "\nOpening your browser to sign in. If it does not open, visit:\n  {url}\n{}",
                    if js::truthy(instructions) {
                        format!("{}\n", js::string(instructions))
                    } else {
                        String::new()
                    }
                ));
                open_browser(&url);
            }
            Some("device_code") => out(&format!(
                "\nVisit {} and enter the code: {}\n",
                js::string(event.get("verificationUri")),
                js::string(event.get("userCode"))
            )),
            _ => out(&format!("{}\n", js::string(event.get("message")))),
        }
    }
}

/// `interaction().prompt(request)`. A prompt the flow cancels (the browser sign-in won the race)
/// releases the terminal at once.
fn answer(prompt: &Value, signal: &AuthSignal) -> Result<String, String> {
    let message = js::string(prompt.get("message"));
    let placeholder = prompt.get("placeholder");
    let query = if js::truthy(placeholder) {
        format!("{message} ({}): ", js::string(placeholder))
    } else {
        format!("{message}: ")
    };
    match prompt["type"].as_str() {
        Some("secret") => secret(&query, Some(signal)),
        Some("select") => {
            let options = prompt["options"].as_array().cloned().unwrap_or_default();
            let menu: Vec<(String, String)> = options
                .iter()
                .map(|option| {
                    let description = option.get("description").filter(|value| !value.is_null());
                    (
                        js::string(option.get("label")),
                        description.map_or_else(String::new, |value| js::string(Some(value))),
                    )
                })
                .collect();
            let picked = choose(&message, &menu)?.ok_or("cancelled")?;
            Ok(js::string(options[picked].get("id")))
        }
        _ => ask(&query, Some(signal)),
    }
}

/// `ask(question, signal)`: one line, trimmed.
fn ask(query: &str, signal: Option<&AuthSignal>) -> Result<String, String> {
    prompt::question(query, signal).map(|answer| js_trim(&answer).to_owned())
}

/// `choose(title, options)`: a numbered menu. A mistyped number is asked again; only an empty
/// answer cancels (`None`).
fn choose(title: &str, options: &[(String, String)]) -> Result<Option<usize>, String> {
    out(&format!("\n{title}\n"));
    let width = options
        .iter()
        .map(|(label, _)| js::length(label))
        .max()
        .unwrap_or(0);
    for (index, (label, detail)) in options.iter().enumerate() {
        out(&format!(
            "  {}. {}  {detail}\n",
            pad_start(&(index + 1).to_string(), 2),
            pad_end(label, width)
        ));
    }
    loop {
        let raw = ask(&format!("Choose 1-{}: ", options.len()), None)?;
        if raw.is_empty() {
            return Ok(None);
        }
        if let Some(index) = chosen(&raw, options.len()) {
            return Ok(Some(index));
        }
        out(&format!("“{raw}” is not one of 1-{}.\n", options.len()));
    }
}

/// `Number(raw)` as one of `count` menu numbers: its index when it is an integer in range.
fn chosen(raw: &str, count: usize) -> Option<usize> {
    let answer = js::js_number(raw);
    #[allow(clippy::cast_precision_loss)]
    let valid = answer.fract() == 0.0 && answer >= 1.0 && answer <= count as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    valid.then(|| answer as usize - 1)
}

/// `secret(question, signal)`: read without echo, one `*` per character, trimmed. Without a
/// terminal it is an ordinary question.
fn secret(query: &str, signal: Option<&AuthSignal>) -> Result<String, String> {
    if !std::io::stdin().is_terminal() {
        return ask(query, signal);
    }
    out(query);
    let raw = RawMode::enable()?;
    let mut value = String::new();
    let outcome = loop {
        let Some(key) = prompt::next_key(signal)? else {
            break Err("cancelled".to_owned());
        };
        match secret_input(&mut value, &key) {
            Typed::Done => {
                prompt::discard_pending();
                break Ok(js_trim(&value).to_owned());
            }
            Typed::Cancelled => break Err("cancelled".to_owned()),
            Typed::Echo(text) => out(text),
            Typed::Nothing => {}
        }
    };
    drop(raw);
    out("\n");
    outcome
}

/// What one key does to a secret being typed.
#[derive(Debug, PartialEq)]
enum Typed {
    Done,
    Cancelled,
    Echo(&'static str),
    Nothing,
}

/// One key of `secret`'s `onData`, read as the character Node receives for it: Enter (or Ctrl+M,
/// CR, or Ctrl+J, LF) ends, Ctrl+C cancels, Backspace (or Ctrl+H) removes a character, and
/// printable characters are kept. Other keys are ignored.
fn secret_input(value: &mut String, key: &KeyEvent) -> Typed {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let erase = |value: &mut String| {
        if value.pop().is_some() {
            Typed::Echo("\u{8} \u{8}")
        } else {
            Typed::Nothing
        }
    };
    match key.code {
        KeyCode::Enter => Typed::Done,
        KeyCode::Char('j' | 'm') if control && !alt => Typed::Done,
        KeyCode::Char('c') if control && !alt => Typed::Cancelled,
        KeyCode::Char('h') if control && !alt => erase(value),
        KeyCode::Backspace => erase(value),
        // AltGr arrives as Ctrl+Alt and types its character.
        KeyCode::Char(character) if control == alt && character >= ' ' => {
            value.push(character);
            Typed::Echo("*")
        }
        _ => Typed::Nothing,
    }
}

/// `openBrowser(url)`: best effort; the printed URL is the fallback.
fn open_browser(url: &str) {
    let (program, args): (&str, &[&str]) = if cfg!(windows) {
        ("rundll32", &["url.dll,FileProtocolHandler", url])
    } else if cfg!(target_os = "macos") {
        ("open", &[url])
    } else {
        ("xdg-open", &[url])
    };
    // Detached, so it has no console window either (`hidden` would be replaced by `detach`).
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    nosh_sys::process::detach(&mut command);
    let _ = command.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_answers_are_whole_numbers_in_range() {
        assert_eq!(chosen("2", 3), Some(1));
        assert_eq!(chosen("0x2", 3), Some(1), "Number() reads hex");
        assert_eq!(chosen("3.0", 3), Some(2));
        assert_eq!(chosen("1.5", 3), None);
        assert_eq!(chosen("4", 3), None);
        assert_eq!(chosen("0", 3), None);
        assert_eq!(chosen("two", 3), None);
    }

    #[test]
    fn secrets_mask_characters_and_erase_them() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut value = String::new();
        assert_eq!(
            secret_input(&mut value, &key(KeyCode::Backspace)),
            Typed::Nothing
        );
        assert_eq!(
            secret_input(&mut value, &key(KeyCode::Char('é'))),
            Typed::Echo("*")
        );
        assert_eq!(secret_input(&mut value, &key(KeyCode::Tab)), Typed::Nothing);
        assert_eq!(
            secret_input(&mut value, &key(KeyCode::Char('x'))),
            Typed::Echo("*")
        );
        assert_eq!(
            secret_input(&mut value, &key(KeyCode::Backspace)),
            Typed::Echo("\u{8} \u{8}")
        );
        assert_eq!(value, "é");
        assert_eq!(
            secret_input(
                &mut value,
                &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Typed::Cancelled
        );
        assert_eq!(secret_input(&mut value, &key(KeyCode::Enter)), Typed::Done);
        assert_eq!(
            secret_input(
                &mut value,
                &KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)
            ),
            Typed::Done,
            "CR ends the secret"
        );
    }

    #[test]
    fn provider_names_sort_case_insensitively() {
        let mut names = ["mistral", "Cerebras", "azure", "Amazon Bedrock"];
        names.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(names, ["Amazon Bedrock", "azure", "Cerebras", "mistral"]);
    }
}
