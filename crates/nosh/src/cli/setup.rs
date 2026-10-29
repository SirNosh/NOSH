//! `setup` in `main.ts`: the guided first run.

use super::config::Config;
use super::doctor::{clean, run};
use super::{Cli, js_trim, out};
use base64::Engine;
use nosh_agent::pi::node_executable;
use nosh_wire::js;
use serde_json::{Map, Value};
use std::io::IsTerminal;

const WORDMARK: &str = "\
███╗   ██╗ ██████╗ ███████╗██╗  ██╗
████╗  ██║██╔═══██╗██╔════╝██║  ██║
██╔██╗ ██║██║   ██║███████╗███████║
██║╚██╗██║██║   ██║╚════██║██╔══██║
██║ ╚████║╚██████╔╝███████║██║  ██║
╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝";

/// The names of `providers`, joined with `, `.
fn names(providers: &[Value]) -> String {
    let names: Vec<String> = providers
        .iter()
        .map(|provider| js::string(provider.get("name")))
        .collect();
    names.join(", ")
}

/// The setup row for `node --version` (`NOSH_NODE`, else PATH): it must report 22.19 or later.
fn node_check() -> (bool, String) {
    let node = run(node_executable(), &["--version"]);
    if !node.ok {
        return (
            false,
            "not found; install Node 22.19+ from https://nodejs.org".into(),
        );
    }
    let version = js_trim(&node.stdout);
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.split('.').map(js::js_number);
    let major = parts.next().unwrap_or(f64::NAN);
    let minor = parts.next().unwrap_or(f64::NAN);
    if major > 22.0 || (major == 22.0 && minor >= 19.0) {
        (true, version.to_owned())
    } else {
        (
            false,
            format!("found {version}; install Node 22.19+ from https://nodejs.org"),
        )
    }
}

impl Cli {
    /// `setup()`: prerequisites, state, a model account, the daemon and the default model, with a
    /// fix for every failed check. An existing config is never overwritten.
    pub(super) fn setup(&self) -> Result<(), String> {
        let data_directory = self.state_root.join("data");
        std::fs::create_dir_all(&data_directory).map_err(|error| error.to_string())?;
        if !self.config_path.exists() {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
            let mut fields = Map::new();
            fields.insert(
                "dataDirectory".into(),
                Value::from(data_directory.to_string_lossy().as_ref()),
            );
            fields.insert("port".into(), Value::from(4321));
            fields.insert(
                "bootstrapToken".into(),
                Value::from(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)),
            );
            fields.insert("currentProjectId".into(), Value::Null);
            self.save_config(&Config::new(fields))?;
        }
        if cfg!(windows)
            && let Some(user) = std::env::var_os("USERNAME").filter(|user| !user.is_empty())
        {
            let mut grant = user;
            grant.push(":(OI)(CI)F");
            let _ = nosh_sys::process::hidden("icacls.exe")
                .arg(&self.state_root)
                .args(["/inheritance:r", "/grant:r"])
                .arg(grant)
                .output();
        }
        out(&format!(
            "{WORDMARK}\n   autonomous research you can verify\n\nWelcome to NOSH. Setup checks your tools, connects a model account, and starts the local daemon.\nState lives in {}\n\nChecking prerequisites\n",
            self.state_root.display()
        ));
        let (node_ok, node_detail) = node_check();
        let git = run("git", &["--version"]);
        let checks = [
            ("Node >= 22.19", node_ok, node_detail),
            (
                "Git",
                git.ok,
                if git.ok {
                    clean(&git.stdout)
                } else {
                    "install Git from https://git-scm.com".into()
                },
            ),
        ];
        for (name, ok, detail) in &checks {
            out(&if *ok {
                format!("  ✓ {name}  {detail}\n")
            } else {
                format!("  ✗ {name}\n      → {detail}\n")
            });
        }
        if checks.iter().any(|(_, ok, _)| !ok) {
            return Err("fix the prerequisites above, then run `nosh setup` again".into());
        }
        let mut config = self.load_config()?;
        // A model account comes first: a ChatGPT/Claude subscription or an API key, stored where
        // Pi stores it.
        let mut connected = self.connected_providers()?;
        let mut newly_connected = false;
        if !connected.is_empty() {
            out(&format!("  ✓ Model access  {}\n", names(&connected)));
        } else if std::io::stdin().is_terminal() && !self.flag("--yes") {
            out(
                "\nConnect a model account. NOSH uses it for every research session; credentials stay in Pi's store and are never shown.\n",
            );
            while connected.is_empty() && self.connect_model_provider()? {
                connected = self.connected_providers()?;
                newly_connected = !connected.is_empty();
            }
        }
        out("\n");
        // A daemon started before this sign-in has not loaded the new credential.
        if newly_connected && self.healthy(&config) {
            self.stop(&config)?;
        }
        self.start(&config)?;
        if connected.is_empty() {
            out(
                "\nSetup is not finished: no model account is connected, so research sessions cannot run yet.\nNext:\n  nosh login       connect a ChatGPT/Claude subscription or an API key\n  nosh open        then open the terminal UI (press ctrl+o to open or create a project)\n",
            );
            return Ok(());
        }
        config = self.choose_default_model(config)?;
        out(&format!(
            "\nReady. Next:\n  nosh open        open the terminal UI (press ctrl+o to open or create a project)\n  nosh doctor      re-run these checks any time\n  nosh model       show or change the default model{}\n",
            config
                .default_model()
                .map_or_else(String::new, |model| format!(" (now {model})"))
        ));
        Ok(())
    }
}
