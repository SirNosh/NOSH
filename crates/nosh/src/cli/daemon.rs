//! `start`, `stop`, `status` and `logs` in `main.ts`.

use super::config::Config;
use super::{Cli, err, join, out};
use nosh_server::noshd::Installation;
use nosh_wire::js;
use reqwest::Method;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `<dataDirectory>/logs/noshd.log`: the daemon's stdout and stderr.
fn log_path(config: &Config) -> Result<PathBuf, String> {
    Ok(config.data_directory()?.join("logs").join("noshd.log"))
}

impl Cli {
    /// `start(config)`: this executable's `nosh daemon`, detached, its output appended to the log.
    /// The bootstrap token goes in the environment, never on the command line.
    pub(super) fn start(&self, config: &Config) -> Result<(), String> {
        if self.healthy(config) {
            out(&format!("noshd already running at {}\n", config.url()));
            return Ok(());
        }
        // The daemon is built in, but it cannot run without the installation it loads.
        if Installation::find().is_err() {
            return Err(
                "noshd build is missing; install a NOSH distribution or run the repository build"
                    .into(),
            );
        }
        let log_path = log_path(config)?;
        let failed = |error: std::io::Error| error.to_string();
        std::fs::create_dir_all(log_path.parent().unwrap_or(&log_path)).map_err(failed)?;
        let log = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_path)
            .map_err(failed)?;
        let executable = std::env::current_exe().map_err(failed)?;
        let mut command = Command::new(&executable);
        command
            .arg("daemon")
            .arg(format!(
                "--data-dir={}",
                js::string(config.get("dataDirectory"))
            ))
            .arg(format!("--port={}", js::string(config.get("port"))))
            .env(
                "NOSH_BOOTSTRAP_TOKEN",
                js::string(config.get("bootstrapToken")),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(failed)?)
            .stderr(log);
        if let Some(model) = config.default_model() {
            command.env("NOSH_DEFAULT_MODEL", model);
        }
        nosh_sys::process::detach(&mut command);
        // A caller capturing this command's output must not wait for the daemon too.
        nosh_sys::process::disable_stdio_inheritance();
        let spawned = command.spawn();
        out("Starting noshd…\n");
        let mut child = spawned.map_err(|error| {
            format!(
                "noshd launch failed: {}",
                nosh_sys::process::spawn_error_message(&executable.to_string_lossy(), &error)
            )
        })?;
        // Cold Pi imports on Windows can take long.
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if self.healthy(config) {
                out(&format!("noshd started at {}\n", config.url()));
                return Ok(());
            }
            if child.try_wait().ok().flatten().is_some() {
                return Err(format!(
                    "noshd exited during startup; inspect {}",
                    log_path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "noshd did not become healthy within 90 seconds; inspect {}",
            log_path.display()
        ))
    }

    /// `stop(config)`. With `--if-idle` (the npm launcher's update) it exits 2 without stopping
    /// while any Project has active work, and 3 when no daemon runs.
    pub(super) fn stop(&self, config: &Config) -> Result<(), String> {
        let if_idle = self.flag("--if-idle");
        if !self.healthy(config) {
            if if_idle {
                self.exit_code.set(3);
            }
            out("noshd is not running\n");
            return Ok(());
        }
        if if_idle {
            let activity = self.api(config, Method::GET, "/activity", None)?;
            let active = activity
                .get("active")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !active.is_empty() {
                let lines: Vec<String> = active
                    .iter()
                    .map(|entry| {
                        format!(
                            "  {}: {}",
                            js::string(entry.get("projectId")),
                            join(entry.get("active"), ", ")
                        )
                    })
                    .collect();
                err(&format!(
                    "noshd has active work; finish or stop it first:\n{}\n",
                    lines.join("\n")
                ));
                self.exit_code.set(2);
                return Ok(());
            }
        }
        self.api(config, Method::POST, "/shutdown", Some("{}".into()))?;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(200));
            if !self.healthy(config) {
                out("noshd stopped cleanly\n");
                return Ok(());
            }
        }
        Err(format!(
            "noshd did not stop cleanly; inspect {}",
            log_path(config)?.display()
        ))
    }

    /// `status(config)`.
    pub(super) fn status(&self, config: &Config) {
        if self.healthy(config) {
            out(&format!("running {}\n", config.url()));
        } else {
            out("stopped\n");
        }
    }

    /// `logs(config)`: the last 200 elements of the log split at line breaks, so a final newline
    /// shows as an empty last line.
    pub(super) fn logs(&self, config: &Config) -> Result<(), String> {
        let path = log_path(config)?;
        if !path.exists() {
            out("No daemon log\n");
            return Ok(());
        }
        let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect();
        out(&format!(
            "{}\n",
            lines[lines.len().saturating_sub(200)..].join("\n")
        ));
        Ok(())
    }
}
