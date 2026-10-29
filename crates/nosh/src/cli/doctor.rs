//! `doctor`, `run` and `clean` in `main.ts`, and `apps/cli/src/backup-doctor.ts`.

use super::config::Config;
use super::{Cli, js_trim, out};
use nosh_server::noshd::Installation;
use nosh_wire::js;
use serde_json::Value;
use std::path::{Component, Path};
use std::sync::LazyLock;
use std::time::Duration;

/// `CommandResult`: whether a helper program exited 0, and its stdout.
pub struct CommandResult {
    pub ok: bool,
    pub stdout: String,
}

/// `DoctorCheck`: name, ok, detail.
pub type DoctorCheck = (String, bool, String);

fn check(name: &str, ok: bool, detail: &str) -> DoctorCheck {
    (name.to_owned(), ok, detail.to_owned())
}

/// `gitDoctorChecks(repositoryRoot, run)`: Git itself is always checked first, and worktrees only
/// in the registered Project's root, never in the caller's cwd.
pub fn git_doctor_checks(
    repository_root: Option<&str>,
    run: &mut dyn FnMut(&str, &[&str]) -> CommandResult,
) -> Vec<DoctorCheck> {
    if !run("git", &["--version"]).ok {
        return vec![
            check("Git", false, "not available"),
            check("Git worktrees", false, "Git unavailable"),
        ];
    }
    let Some(root) = repository_root.filter(|root| !root.is_empty()) else {
        return vec![
            check("Git", true, "available"),
            check("Git worktrees", true, "not checked; no current Project"),
        ];
    };
    let worktrees = run("git", &["-C", root, "worktree", "list"]);
    let count = worktrees
        .stdout
        .split('\n')
        .filter(|line| !js_trim(line).is_empty())
        .count();
    let detail = if worktrees.ok {
        format!("{count} available")
    } else {
        "unavailable".into()
    };
    vec![
        check("Git", true, "available"),
        check("Git worktrees", worktrees.ok, &detail),
    ]
}

/// `run(command, args)`: a hidden helper process with a 10 s limit; a missing program or a
/// timeout is not ok.
pub(super) fn run(command: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> CommandResult {
    let mut command = nosh_sys::process::hidden(command);
    match nosh_sys::process::output_with_timeout(command.args(args), Duration::from_secs(10)) {
        Ok((status, output)) => CommandResult {
            ok: status.and_then(|status| status.code()) == Some(0),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        },
        Err(_) => CommandResult {
            ok: false,
            stdout: String::new(),
        },
    }
}

/// `clean(value)`: control-character runs become one space, then trimmed and cut to 200 UTF-16
/// units.
pub(super) fn clean(value: &str) -> String {
    static CONTROL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[\x00-\x1f]+").expect("pattern compiles"));
    js::slice_to(js_trim(&CONTROL.replace_all(value, " ")), 200)
}

/// `!path.relative(from, to).startsWith("..")` with Node's win32 `relative`: compared
/// case-insensitively, and a path on another root is returned whole (absolute, so not climbing).
fn stays_inside(from: &Path, to: &Path) -> bool {
    let key = |component: Component<'_>| component.as_os_str().to_string_lossy().to_lowercase();
    let from: Vec<String> = from.components().map(key).collect();
    let to: Vec<String> = to.components().map(key).collect();
    if from.first() != to.first() {
        return true;
    }
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    common == from.len() && !to.get(common).is_some_and(|next| next.starts_with(".."))
}

impl Cli {
    /// `doctor(config)`: one `PASS`/`FAIL` row per check; any failure exits 1. The Bun row is gone
    /// with Bun (the terminal UI is built in).
    pub(super) fn doctor(&self, config: &Config) -> Result<(), String> {
        let mut checks = Vec::new();
        let current = match config.current_project_id() {
            Some(_) => self.projects(config)?.into_iter().find(|project| {
                js::strict_equal(project.get("projectId"), config.get("currentProjectId"))
            }),
            None => None,
        };
        let root = current
            .as_ref()
            .and_then(|project| project.get("repositoryRoot"))
            .filter(|root| js::truthy(Some(root)))
            .map(|root| js::string(Some(root)));
        checks.extend(git_doctor_checks(root.as_deref(), &mut |command, args| {
            run(command, args)
        }));
        let pi = run(if cfg!(windows) { "where.exe" } else { "which" }, &["pi"]);
        let found = clean(&pi.stdout);
        checks.push(check(
            "Pi CLI",
            pi.ok,
            if found.is_empty() {
                "not found"
            } else {
                &found
            },
        ));
        let package = Installation::find()
            .is_ok_and(|installation| installation.package_path.join("package.json").exists());
        checks.push(check("NOSH Pi package", package, "native package metadata"));
        let accounts = self.connected_providers().unwrap_or_default();
        let names: Vec<String> = accounts
            .iter()
            .map(|provider| js::string(provider.get("name")))
            .collect();
        checks.push(check(
            "Model access",
            !accounts.is_empty(),
            &if names.is_empty() {
                "none; run nosh login".to_owned()
            } else {
                names.join(", ")
            },
        ));
        let model = config.get("defaultModel");
        checks.push(check(
            "Default model",
            js::truthy(model),
            &match model {
                None | Some(Value::Null) => "none; run nosh model set <provider/id:level>".into(),
                model => js::string(model),
            },
        ));
        let running = self.healthy(config);
        let database = running && config.data_directory()?.join("host.sqlite").exists();
        checks.push(check(
            "daemon/database",
            database,
            if running {
                "healthy"
            } else {
                "stopped; run nosh start"
            },
        ));
        if self.flag("--wsl") {
            // wsl.exe writes UTF-16LE, read as UTF-8 as Node reads it.
            let wsl = run("wsl.exe", &["--list", "--quiet"]);
            let found = clean(&wsl.stdout);
            checks.push(check(
                "WSL",
                wsl.ok && !js_trim(&wsl.stdout).is_empty(),
                if found.is_empty() {
                    "not available"
                } else {
                    &found
                },
            ));
        }
        if self.flag("--gpu") {
            let gpu = run(
                "nvidia-smi",
                &["--query-gpu=name,driver_version", "--format=csv,noheader"],
            );
            let found = clean(&gpu.stdout);
            checks.push(check(
                "NVIDIA telemetry",
                gpu.ok,
                if found.is_empty() {
                    "not available"
                } else {
                    &found
                },
            ));
        }
        let schemas = nosh_wire::schema_uris().len();
        checks.push(check(
            "schema/protocol",
            schemas > 10,
            &format!("{schemas} registered schema URIs; protocol v1"),
        ));
        let local = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map_or_else(|| self.state_root.clone(), Into::into);
        let local = nosh_sys::paths::resolve(&local).map_err(|error| error.to_string())?;
        checks.push(check(
            "protected storage",
            stays_inside(&local, &self.state_root),
            &self.state_root.to_string_lossy(),
        ));
        for (name, ok, detail) in &checks {
            out(&format!(
                "{}\t{name}\t{detail}\n",
                if *ok { "PASS" } else { "FAIL" }
            ));
        }
        if checks.iter().any(|(_, ok, _)| !ok) {
            self.exit_code.set(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_folds_control_runs_trims_and_cuts() {
        assert_eq!(
            clean("C:\\a\\pi\r\nC:\\a\\pi.cmd\r\n"),
            "C:\\a\\pi C:\\a\\pi.cmd"
        );
        assert_eq!(clean("U\0b\0u\0"), "U b u");
        assert_eq!(js::length(&clean(&"x".repeat(300))), 200);
    }
}
