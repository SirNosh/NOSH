//! The `nosh` CLI (`apps/cli/src/main.ts`): argument parsing, routing, and the top-level catch.
//!
//! Structural changes from TypeScript (cli.md §13.1):
//! - Pi sign-in runs in the Node sidecar (`nosh_agent::pi::PiAdapter`), not in-process.
//! - The setup Node check runs `node --version` (`NOSH_NODE`, else PATH) instead of reading the
//!   running interpreter.
//! - Bun is gone: no Bun rows in `setup` and `doctor`, no Bun errors in `open`.
//! - The daemon is this executable's `nosh daemon`, and the terminal UI runs in-process.
//!
//! Async work (daemon requests, the sidecar) runs one `block_on` at a time on a runtime that is
//! otherwise idle, so the terminal UI can build its own runtime in between.

mod backup;
mod config;
mod daemon;
mod doctor;
mod http;
mod login;
mod model;
mod project;
mod prompt;
mod setup;
mod tui;
mod update;

pub use doctor::{CommandResult, DoctorCheck, git_doctor_checks};

use nosh_agent::pi::PiAdapter;
use serde_json::Value;
use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

/// One CLI process: its arguments' flags, state paths, and the exit code it ends with.
struct Cli {
    /// `--key[=value]` arguments, the last of a key winning. Every command reads them, whatever it is.
    flags: HashMap<String, String>,
    state_root: PathBuf,
    config_path: PathBuf,
    /// The Pi sidecar, started on first use (`adapter()` in `login.ts`).
    pi: OnceCell<PiAdapter>,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    /// `process.exitCode`: commands set it and carry on.
    exit_code: Cell<i32>,
}

/// Runs `nosh <args>` (the module body of `main.ts`) and returns the exit code. A failed command
/// prints `nosh: <message>` and exits 1.
pub fn run(args: &[String]) -> i32 {
    let code = match Cli::new(args) {
        Ok(cli) => {
            let words: Vec<String> = args
                .iter()
                .filter(|arg| !arg.starts_with("--"))
                .cloned()
                .collect();
            if let Err(message) = cli.route(&words) {
                err(&format!("nosh: {message}\n"));
                cli.exit_code.set(1);
            }
            let Cli {
                pi,
                runtime,
                exit_code,
                ..
            } = cli;
            // The sidecar's process handle is released inside the runtime that spawned it.
            {
                let _entered = runtime.enter();
                drop(pi);
            }
            // A terminal read Pi abandoned may still block its thread, as Node exits past an open
            // readline.
            runtime.shutdown_background();
            exit_code.get()
        }
        Err(message) => {
            err(&format!("nosh: {message}\n"));
            1
        }
    };
    let _ = std::io::stdout().flush();
    code
}

impl Cli {
    fn new(args: &[String]) -> Result<Self, String> {
        // `LOCALAPPDATA || APPDATA || join(homedir(), ".nosh")`: an empty variable counts as unset.
        let base = ["LOCALAPPDATA", "APPDATA"]
            .iter()
            .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
            .map_or_else(
                || std::env::home_dir().unwrap_or_default().join(".nosh"),
                PathBuf::from,
            );
        let state_root =
            nosh_sys::paths::resolve(&base.join("NOSH")).map_err(|error| error.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        // Node's fetch ignores HTTP_PROXY; reqwest would not.
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            flags: flags(args),
            config_path: state_root.join("config.json"),
            state_root,
            pi: OnceCell::new(),
            runtime,
            client,
            exit_code: Cell::new(0),
        })
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// `route(words)`: the first matching rule wins.
    fn route(&self, words: &[String]) -> Result<(), String> {
        let word = |index: usize| words.get(index).map(String::as_str);
        // A word's truthiness: absent and empty are both false.
        let given = |index: usize| word(index).filter(|word| !word.is_empty());
        let (command, subcommand, argument, backup_id) = (given(0), word(1), given(2), given(3));
        if command.is_none() || command == Some("help") || self.flag("--help") {
            update::help();
            return Ok(());
        }
        match command {
            Some("setup") => return self.setup(),
            // `login --list` arrives as a flag: words never start with `--`.
            Some("login") => return self.login_command(),
            Some("logout") if given(1).is_some() => return self.logout_command(&words[1]),
            Some("update") => {
                update::update();
                return Ok(());
            }
            Some("stop") if self.flag("--if-idle") && !self.config_path.exists() => {
                self.exit_code.set(3);
                out("noshd is not running\n");
                return Ok(());
            }
            _ => {}
        }
        let config = self.load_config()?;
        match (command, subcommand, argument) {
            (Some("model"), _, _) => self.model(&config, subcommand, word(2)),
            (Some("start"), _, _) => self.start(&config),
            (Some("stop"), _, _) => self.stop(&config),
            (Some("status"), _, _) => {
                self.status(&config);
                Ok(())
            }
            (Some("open" | "tui"), _, _) => self.open_terminal(&config),
            (Some("doctor"), _, _) => self.doctor(&config),
            (Some("logs"), _, _) => self.logs(&config),
            (Some("project"), Some("list"), _) => self.project_list(&config),
            (Some("project"), Some("open"), Some(path)) => self.project_open(&config, path),
            (Some("mission"), Some("list"), _) => self.mission_list(&config),
            (Some("mission"), Some("status"), Some(id)) => self.mission_status(&config, id),
            (Some("job"), Some("list"), _) => self.job_list(&config),
            (Some("backup"), Some("restore"), Some(selector)) if backup_id.is_some() => {
                self.backup_restore(&config, selector, &words[3])
            }
            (Some("backup"), Some(selector), _) if !selector.is_empty() => {
                self.backup(&config, selector)
            }
            _ => Err(format!(
                "Unknown command: {}; run nosh help",
                words.join(" ")
            )),
        }
    }
}

/// `flags`: every `--` argument, at any position, split at its first `=` (a bare flag's value is
/// empty); the last occurrence of a key wins.
fn flags(args: &[String]) -> HashMap<String, String> {
    args.iter()
        .filter(|arg| arg.starts_with("--"))
        .map(|arg| match arg.split_once('=') {
            Some((key, value)) => (key.to_owned(), value.to_owned()),
            None => (arg.clone(), String::new()),
        })
        .collect()
}

/// `process.stdout.write`. Errors are ignored, as Node ignores a closed pipe.
fn out(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// `process.stderr.write`.
fn err(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// `array.join(separator)` for a JSON array: null elements become empty; a non-array joins nothing.
fn join(values: Option<&Value>, separator: &str) -> String {
    values
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::Null => String::new(),
                    item => nosh_wire::js::string(Some(item)),
                })
                .collect::<Vec<_>>()
                .join(separator)
        })
        .unwrap_or_default()
}

/// `text.padEnd(width)`, in UTF-16 code units.
fn pad_end(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(nosh_wire::js::length(text));
    format!("{text}{}", " ".repeat(fill))
}

/// `text.padStart(width)`, in UTF-16 code units.
fn pad_start(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(nosh_wire::js::length(text));
    format!("{}{text}", " ".repeat(fill))
}

/// `text.trim()`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(nosh_wire::js::is_js_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn flags_split_at_the_first_equals_and_the_last_one_wins() {
        let parsed = flags(&args(&[
            "--model=a=b=c",
            "status",
            "--yes",
            "--if-idle=1",
            "--if-idle",
            "-h",
        ]));
        assert_eq!(parsed["--model"], "a=b=c");
        assert_eq!(parsed["--yes"], "");
        assert_eq!(parsed["--if-idle"], "");
        assert_eq!(parsed.len(), 3, "single-dash arguments are words");
    }
}
