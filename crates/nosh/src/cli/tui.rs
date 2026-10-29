//! `openTerminal` in `main.ts` (`nosh open` and `nosh tui`). The terminal UI runs in this process,
//! outside the CLI's runtime; the handoff file still carries the Project it ends on.

use super::config::Config;
use super::{Cli, err};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;

impl Cli {
    /// `openTerminal(config)`: the daemon, then the terminal UI. A UI failure prints its own
    /// message, without the `nosh: ` prefix, and exits 1.
    pub(super) fn open_terminal(&self, config: &Config) -> Result<(), String> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(
                "nosh tui requires an interactive terminal; use nosh help for scripting commands"
                    .into(),
            );
        }
        self.start(config)?;
        let handoff = self.state_root.join("tui-session.json");
        let text = |key| config.get(key).and_then(Value::as_str).map(str::to_owned);
        // The former `NOSH_TUI_CONFIG`; `nosh_tui::run` restores the terminal before returning.
        let outcome = nosh_tui::run(nosh_tui::TuiConfig {
            base_url: config.url(),
            bootstrap_token: text("bootstrapToken"),
            session_token: None,
            current_project_id: text("currentProjectId"),
            handoff_path: Some(handoff.clone()),
        });
        if let Err(message) = outcome {
            err(&format!("{message}\n"));
            self.exit_code.set(1);
        }
        self.apply_handoff(&handoff);
        Ok(())
    }

    /// The next `nosh open` (and project-scoped commands) continue in the Project the terminal UI
    /// ended on. The config is read afresh; any failure keeps the configured Project.
    fn apply_handoff(&self, handoff: &Path) {
        let ended = std::fs::read(handoff)
            .ok()
            .and_then(|bytes| nosh_agent::js_json::parse(&String::from_utf8_lossy(&bytes)).ok())
            .and_then(|handoff| handoff.get("currentProjectId").cloned())
            .filter(Value::is_string);
        if let Some(ended) = ended
            && let Ok(latest) = self.load_config()
            && latest.get("currentProjectId") != Some(&ended)
        {
            let _ = self.save_config(&latest.with("currentProjectId", ended));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handoff_moves_the_config_to_the_project_the_ui_ended_on() {
        let state = tempfile::tempdir().unwrap();
        let cli = Cli {
            flags: std::collections::HashMap::new(),
            state_root: state.path().to_owned(),
            config_path: state.path().join("config.json"),
            pi: std::cell::OnceCell::new(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            client: reqwest::Client::new(),
            exit_code: std::cell::Cell::new(0),
        };
        let handoff = state.path().join("tui-session.json");
        // Not canonical, so any rewrite shows.
        let config = "{ \"currentProjectId\": \"prj_a\", \"zeta\": [2, 1] }";
        let applied = |ended: Option<&str>| {
            std::fs::write(&cli.config_path, config).unwrap();
            let _ = std::fs::remove_file(&handoff);
            if let Some(text) = ended {
                std::fs::write(&handoff, text).unwrap();
            }
            cli.apply_handoff(&handoff);
            std::fs::read_to_string(&cli.config_path).unwrap()
        };
        assert_eq!(
            applied(Some("{\"currentProjectId\":\"prj_b\",\"other\":1}")),
            "{\"currentProjectId\":\"prj_b\",\"zeta\":[2,1]}\n"
        );
        for kept in [
            None,
            Some("{nope"),
            // `readFileSync(…, "utf8")` keeps a BOM, which `JSON.parse` refuses.
            Some("\u{feff}{\"currentProjectId\":\"prj_b\"}"),
            Some("{}"),
            Some("{\"currentProjectId\":null}"),
            Some("{\"currentProjectId\":7}"),
            Some("{\"currentProjectId\":\"prj_a\"}"),
        ] {
            assert_eq!(applied(kept), config, "{kept:?}");
        }
    }
}
