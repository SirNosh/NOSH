//! `model`, `applyDefaultModel`, `chooseDefaultModel`, `availableModels` and `preferredLevel` in
//! `main.ts`: the default model every session that selects none uses.

use super::config::Config;
use super::{Cli, join, js_trim, out, pad_start, prompt};
use nosh_wire::js;
use reqwest::Method;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::sync::LazyLock;

/// Models NOSH is tested with, preferred as the default when the account offers them.
const RECOMMENDED_MODELS: [&str; 1] = ["gpt-6-luna"];
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `parseModelSelection(text)` (`@nosh/wire`): provider, id and thinking level.
fn parse_model_selection(text: &str) -> Result<(String, String, Option<String>), String> {
    static PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
        // JavaScript's `\s`, which is not Rust's (Unicode White_Space): U+FEFF in, U+0085 out.
        const SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
        regex::Regex::new(&format!(r"^([^/{SPACE}]+)/([^:{SPACE}]+)(?::([a-z]+))?$"))
            .expect("pattern compiles")
    });
    let found = PATTERN.captures(js_trim(text)).ok_or_else(|| {
        format!("Model must be provider/id or provider/id:thinkingLevel, got \"{text}\"")
    })?;
    let level = found.get(3).map(|level| level.as_str().to_owned());
    if let Some(level) = level.as_deref()
        && !THINKING_LEVELS.contains(&level)
    {
        return Err(invalid_level(level));
    }
    Ok((found[1].to_owned(), found[2].to_owned(), level))
}

/// The message of the `ZodError` `modelSelectionSchema.parse` throws for a level outside its enum:
/// the issues as `JSON.stringify(issues, null, 2)`, keys in zod's order. `level` is `[a-z]+`.
fn invalid_level(level: &str) -> String {
    let options: Vec<String> = THINKING_LEVELS
        .iter()
        .map(|option| format!("      \"{option}\""))
        .collect();
    let expected: Vec<String> = THINKING_LEVELS
        .iter()
        .map(|option| format!("'{option}'"))
        .collect();
    format!(
        "[\n  {{\n    \"received\": \"{level}\",\n    \"code\": \"invalid_enum_value\",\n    \"options\": [\n{}\n    ],\n    \"path\": [\n      \"thinkingLevel\"\n    ],\n    \"message\": \"Invalid enum value. Expected {}, received '{level}'\"\n  }}\n]",
        options.join(",\n"),
        expected.join(" | ")
    )
}

/// `preferredLevel(entry)`: `low` when offered, else the first level, else none.
fn preferred_level(entry: &Value) -> String {
    let levels = entry
        .get("thinkingLevels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if levels.iter().any(|level| level == "low") {
        return "low".into();
    }
    levels
        .first()
        .filter(|level| !level.is_null())
        .map_or_else(String::new, |level| js::string(Some(level)))
}

/// `entry.thinkingLevels?.length`: a non-empty array.
fn has_levels(entry: &Value) -> bool {
    entry
        .get("thinkingLevels")
        .and_then(Value::as_array)
        .is_some_and(|levels| !levels.is_empty())
}

/// `Math.min(len, Math.max(1, Number(answer.trim() || fallback))) - 1` as an index of one of `len`
/// items; `None` where JavaScript's index would find no item (NaN or a fraction).
fn menu_index(answer: &str, fallback: &str, len: usize) -> Option<usize> {
    let answer = js_trim(answer);
    let number = nosh_wire::js::js_number(if answer.is_empty() { fallback } else { answer });
    #[allow(clippy::cast_precision_loss)]
    let index = js::min(len as f64, js::max(1.0, number)) - 1.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (index.fract() == 0.0 && index >= 0.0).then_some(index as usize)
}

impl Cli {
    /// `availableModels(config)`: the daemon's models, or none when it is down or fails.
    fn available_models(&self, config: &Config) -> Vec<Value> {
        if !self.healthy(config) {
            return Vec::new();
        }
        self.api(config, Method::GET, "/models", None)
            .ok()
            .and_then(|reply| reply.get("models").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    }

    /// `applyDefaultModel(config, text, restart)`: checked against the models Pi lists (when the
    /// daemon answers), saved, and a running daemon restarted (`restart`) or told.
    pub(super) fn apply_default_model(
        &self,
        config: &Config,
        text: &str,
        restart: bool,
    ) -> Result<Config, String> {
        let (provider, id, level) = parse_model_selection(text)?;
        let models = self.available_models(config);
        let found = models
            .iter()
            .find(|entry| entry["provider"] == provider.as_str() && entry["id"] == id.as_str());
        if !models.is_empty() && found.is_none() {
            return Err(format!(
                "{provider}/{id} is not offered by your connected model accounts; run `nosh model list`, or `nosh login` to connect one"
            ));
        }
        if let (Some(entry), Some(level)) = (found, &level)
            && has_levels(entry)
            && !entry["thinkingLevels"]
                .as_array()
                .is_some_and(|levels| levels.iter().any(|offered| offered == level.as_str()))
        {
            return Err(format!(
                "{provider}/{id} supports thinking levels {}",
                join(entry.get("thinkingLevels"), ", ")
            ));
        }
        let model = match &level {
            Some(level) => format!("{provider}/{id}:{level}"),
            None => format!("{provider}/{id}"),
        };
        let next = config.with("defaultModel", Value::String(model.clone()));
        self.save_config(&next)?;
        out(&format!("Default model set to {model}\n"));
        if self.healthy(config) {
            if restart {
                self.stop(&next)?;
                self.start(&next)?;
            } else {
                // Sessions already running keep their model.
                let body = json!({ "model": model }).to_string();
                self.api(&next, Method::PUT, "/default-model", Some(body))?;
                out("The running daemon uses it for every new session.\n");
            }
        }
        Ok(next)
    }

    /// `model(config, action = "show", value)`.
    pub(super) fn model(
        &self,
        config: &Config,
        action: Option<&str>,
        value: Option<&str>,
    ) -> Result<(), String> {
        match action.unwrap_or("show") {
            "show" => {
                let shown = match config.get("defaultModel") {
                    None | Some(Value::Null) => "none (Pi's global default applies; set one with `nosh model set <provider/id:level>`)".to_owned(),
                    model => js::string(model),
                };
                out(&format!("{shown}\n"));
                Ok(())
            }
            "list" => {
                let models = self.available_models(config);
                if models.is_empty() {
                    out(if self.healthy(config) {
                        "Pi lists no authenticated models; log in with `pi` first.\n"
                    } else {
                        "Start the daemon first: nosh start\n"
                    });
                    return Ok(());
                }
                let default = config
                    .get("defaultModel")
                    .and_then(Value::as_str)
                    .and_then(|model| model.split(':').next());
                for entry in &models {
                    let name = format!(
                        "{}/{}",
                        js::string(entry.get("provider")),
                        js::string(entry.get("id"))
                    );
                    let levels = if has_levels(entry) {
                        format!("  [{}]", join(entry.get("thinkingLevels"), ", "))
                    } else {
                        String::new()
                    };
                    let marker = if Some(name.as_str()) == default {
                        "  ← default"
                    } else {
                        ""
                    };
                    out(&format!("{name}{levels}{marker}\n"));
                }
                Ok(())
            }
            "set" if value.is_some_and(|value| !value.is_empty()) => self
                .apply_default_model(config, value.unwrap_or_default(), self.flag("--restart"))
                .map(drop),
            _ => Err(
                "Use nosh model [show|list|set <provider/id[:thinkingLevel]> [--restart]]".into(),
            ),
        }
    }

    /// `chooseDefaultModel(config)`: `--model`, else the saved default (unless `--reselect`), else
    /// an account and model picked from what Pi lists, recommended model first.
    pub(super) fn choose_default_model(&self, config: Config) -> Result<Config, String> {
        if let Some(requested) = self.flags.get("--model").filter(|model| !model.is_empty()) {
            return self.apply_default_model(&config, requested, true);
        }
        if let Some(model) = config.default_model()
            && !self.flag("--reselect")
        {
            out(&format!(
                "\nDefault model: {model} (change with `nosh model set`)\n"
            ));
            return Ok(config);
        }
        let models = self.available_models(&config);
        let Some(first) = models.first() else {
            out(
                "\nPi lists no authenticated models yet; after logging in, run `nosh model set <provider/id:level>`.\n",
            );
            return Ok(config);
        };
        if !std::io::stdin().is_terminal() || self.flag("--yes") {
            out(&format!(
                "\nNo default model chosen (non-interactive). Run `nosh model set <provider/id:level>`; for example {}/{}:{}\n",
                js::string(first.get("provider")),
                js::string(first.get("id")),
                preferred_level(first)
            ));
            return Ok(config);
        }
        let mut providers: Vec<String> = Vec::new();
        for entry in &models {
            let provider = js::string(entry.get("provider"));
            if !providers.contains(&provider) {
                providers.push(provider);
            }
        }
        let mut provider = providers[0].clone();
        if providers.len() > 1 {
            out("\nWhich connected account should NOSH use by default?\n");
            for (index, name) in providers.iter().enumerate() {
                out(&format!(
                    "  {}. {name}\n",
                    pad_start(&(index + 1).to_string(), 2)
                ));
            }
            let answer = prompt::question(
                &format!("Account [1-{}, default 1]: ", providers.len()),
                None,
            )?;
            if let Some(index) = menu_index(&answer, "1", providers.len()) {
                provider.clone_from(&providers[index]);
            }
        }
        // The recommended model is listed first, so the default is visible without scrolling.
        let mut choices: Vec<&Value> = models
            .iter()
            .filter(|entry| js::string(entry.get("provider")) == provider)
            .collect();
        if let Some(preferred) = choices.iter().position(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| RECOMMENDED_MODELS.contains(&id))
        }) {
            let entry = choices.remove(preferred);
            choices.insert(0, entry);
        }
        out(&format!(
            "\nDefault model for NOSH sessions ({provider}):\n"
        ));
        for (index, entry) in choices.iter().enumerate() {
            let id = js::string(entry.get("id"));
            let name = entry.get("name");
            let named = if js::truthy(name) && js::string(name) != id {
                format!("  ({})", js::string(name))
            } else {
                String::new()
            };
            let recommended = if index == 0 { "  ← recommended" } else { "" };
            out(&format!(
                "  {}. {id}{named}{recommended}\n",
                pad_start(&(index + 1).to_string(), 2)
            ));
        }
        let answer = prompt::question(&format!("Model [1-{}, default 1]: ", choices.len()), None)?;
        let picked = menu_index(&answer, "1", choices.len())
            .and_then(|index| choices.get(index))
            .unwrap_or(&choices[0]);
        let fallback = preferred_level(picked);
        let level = if has_levels(picked) {
            let levels = join(picked.get("thinkingLevels"), "/");
            let answer = prompt::question(
                &format!("Thinking level [{levels}, default {fallback}]: "),
                None,
            )?;
            match js_trim(&answer) {
                "" => fallback,
                answer => answer.to_owned(),
            }
        } else {
            String::new()
        };
        let selection = format!(
            "{}/{}{}",
            js::string(picked.get("provider")),
            js::string(picked.get("id")),
            if level.is_empty() {
                String::new()
            } else {
                format!(":{level}")
            }
        );
        self.apply_default_model(&config, &selection, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_answers_follow_javascript_number_arithmetic() {
        let pick = |answer: &str| menu_index(answer, "1", 3).filter(|index| *index < 3);
        assert_eq!(pick(""), Some(0));
        assert_eq!(pick("0"), Some(0));
        assert_eq!(pick("-5"), Some(0));
        assert_eq!(pick("99"), Some(2));
        assert_eq!(pick("Infinity"), Some(2));
        assert_eq!(pick("abc"), None, "NaN falls back to the first item");
        assert_eq!(pick("1.5"), None, "index 0.5 finds no item");
        assert_eq!(pick("2.0"), Some(1));
        assert_eq!(pick("+2"), Some(1));
        assert_eq!(pick("0x2"), Some(1));
        assert_eq!(pick(" 2 "), Some(1));
        assert_eq!(pick("1e1"), Some(2));
    }

    #[test]
    fn model_selections_parse_and_fail_as_typescript_does() {
        assert_eq!(
            parse_model_selection("  a/b:low  ").unwrap(),
            ("a".into(), "b".into(), Some("low".into()))
        );
        assert_eq!(
            parse_model_selection("a/b/c").unwrap(),
            ("a".into(), "b/c".into(), None)
        );
        assert_eq!(
            parse_model_selection("a/b\u{85}c").unwrap(),
            ("a".into(), "b\u{85}c".into(), None),
            "U+0085 is not JavaScript whitespace"
        );
        for bad in [
            "a/b:LOW",
            "a/b:",
            "a/b c",
            "a/b:c:d",
            "nope",
            "a/b\u{FEFF}c",
        ] {
            assert_eq!(
                parse_model_selection(bad).unwrap_err(),
                format!("Model must be provider/id or provider/id:thinkingLevel, got \"{bad}\"")
            );
        }
        let zod = parse_model_selection("a/b:foo").unwrap_err();
        assert!(zod.starts_with("[\n  {\n    \"received\": \"foo\",\n    \"code\": \"invalid_enum_value\",\n    \"options\": [\n      \"off\",\n"));
        assert!(zod.ends_with("\"message\": \"Invalid enum value. Expected 'off' | 'minimal' | 'low' | 'medium' | 'high' | 'xhigh' | 'max', received 'foo'\"\n  }\n]"));
    }
}
