//! The daemon process (`apps/noshd/src/main.ts`), run as `nosh daemon [--data-dir=<dir>]
//! [--port=<port>] [--bootstrap-token=<token>]`.
//!
//! stdout is the daemon log (`nosh logs`): lifecycle and failure events only, never model text or
//! record bodies.

use crate::LocalApiServer;
use base64::Engine;
use nosh_agent::pi::{PiAdapter, PiConfig};
use nosh_orchestration::daemon::{DaemonOptions, NoshDaemon, bridge, parse_model_selection};
use nosh_wire::js::js_number;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The files of a NOSH installation the daemon loads at run time.
pub struct Installation {
    /// `pi-package/`: the Pi package every session loads.
    pub package_path: PathBuf,
    /// `sidecar/pi-host/host.mjs`: the Pi sidecar.
    pub host_script: PathBuf,
    /// The record JSON Schemas prompts point at.
    pub schema_dir: PathBuf,
}

impl Installation {
    pub fn at(root: &Path) -> Self {
        Self {
            package_path: root.join("pi-package"),
            host_script: root.join("sidecar").join("pi-host").join("host.mjs"),
            schema_dir: root.join("conformance").join("schemas").join("json"),
        }
    }

    /// The nearest directory at or above the running executable's that holds the Pi sidecar: the
    /// release layout, or the source tree for a `target/<profile>/nosh` build.
    pub fn find() -> Result<Self, String> {
        Self::find_from(&std::env::current_exe().map_err(|error| error.to_string())?)
    }

    /// [`Installation::find`] from a given executable path.
    pub fn find_from(executable: &Path) -> Result<Self, String> {
        executable
            .ancestors()
            .skip(1)
            .map(Self::at)
            .find(|installation| installation.host_script.is_file())
            .ok_or_else(|| {
                format!(
                    "No NOSH installation (sidecar/pi-host/host.mjs) found above {}",
                    executable.display()
                )
            })
    }
}

/// `main.ts`'s arguments: `--key=value` pairs, the last of a key winning. As with
/// `argument.split("=", 2)`, a value ends at its next `=`.
fn arguments(args: &[String]) -> HashMap<String, String> {
    args.iter()
        .map(|argument| {
            let mut parts = argument.split('=');
            let key = parts.next().unwrap_or_default().to_owned();
            (key, parts.next().unwrap_or_default().to_owned())
        })
        .collect()
}

/// A daemon log line: `<ISO time> <message>`.
fn log(line: &str) {
    let now = nosh_wire::now_utc();
    // A closed stdout must not stop the daemon.
    let _ = writeln!(std::io::stdout().lock(), "{now} {line}");
}

/// The log line for an event operators need, if it is one.
fn operator_line(event: &Value) -> Option<String> {
    static OPERATOR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(error|failed|blocked|halted|exhausted|paused|stopped|closed|completed|accepted|resolved|recovery\.report)$")
            .expect("pattern compiles")
    });
    static VERBOSE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(agent|runtime|record)\.").expect("pattern compiles")
    });
    let kind = event["type"].as_str().unwrap_or_default();
    if !OPERATOR.is_match(kind) || VERBOSE.is_match(kind) {
        return None;
    }
    // Scope members in their schema order, as `Object.entries` lists a stored scope.
    const ORDER: [&str; 8] = [
        "projectId",
        "missionId",
        "directionId",
        "autoresearchId",
        "experimentId",
        "runId",
        "jobId",
        "agentId",
    ];
    let empty = serde_json::Map::new();
    let scope = event["scope"].as_object().unwrap_or(&empty);
    let mut keys: Vec<&String> = scope.keys().collect();
    keys.sort_by_key(|key| {
        ORDER
            .iter()
            .position(|known| known == key)
            .unwrap_or(ORDER.len())
    });
    let scope: Vec<String> = keys
        .into_iter()
        .filter(|key| *key != "projectId")
        .filter_map(|key| Some(format!("{key}={}", scope[key].as_str()?)))
        .collect();
    let message = event["payload"]["message"]
        .as_str()
        .map(|message| format!(" :: {}", nosh_wire::js::slice_to(message, 300)))
        .unwrap_or_default();
    Some(format!("{kind} {}{message}", scope.join(" ")))
}

/// Runs the daemon until a signal or a shutdown request; returns the process exit code.
/// `job_wrapper` is the command that runs native Jobs (`nosh job-wrapper`).
pub fn main(args: &[String], job_wrapper: Vec<OsString>) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    match local.block_on(&runtime, run(args, job_wrapper)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

async fn run(args: &[String], job_wrapper: Vec<OsString>) -> Result<(), String> {
    let args = arguments(args);
    let value = |key: &str| args.get(key).filter(|value| !value.is_empty()).cloned();
    let data_directory = nosh_sys::paths::resolve(Path::new(
        &value("--data-dir").unwrap_or_else(|| ".noshd".into()),
    ))
    .map_err(|error| error.to_string())?;
    let port = js_number(&value("--port").unwrap_or_else(|| "4321".into()));
    if !(port.fract() == 0.0 && (0.0..65536.0).contains(&port)) {
        return Err(format!(
            "options.port should be >= 0 and < 65536. Received type number ({}).",
            nosh_wire::js::number_string(port)
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port as u16;
    let bootstrap_token = value("--bootstrap-token")
        .or_else(|| {
            std::env::var("NOSH_BOOTSTRAP_TOKEN")
                .ok()
                .filter(|token| !token.is_empty())
        })
        .unwrap_or_else(|| {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("the operating system provides randomness");
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        });
    let default_model = match std::env::var("NOSH_DEFAULT_MODEL") {
        Ok(model) if !model.is_empty() => {
            Some(parse_model_selection(&model).map_err(|error| error.to_string())?)
        }
        _ => None,
    };
    let installation = Installation::find()?;

    let (connected, inbox) = bridge();
    let adapter = PiAdapter::spawn(
        PiConfig {
            node: nosh_agent::pi::node_executable(),
            host_script: installation.host_script.clone(),
            env: Vec::new(),
            schema_dir: installation.schema_dir.clone(),
            forward_stderr: true,
        },
        connected.emitter(),
        Some(connected.agent_host()),
        Some(connected.terminal_host()),
    )
    .await?;
    let daemon = NoshDaemon::new(
        DaemonOptions {
            data_directory: data_directory.clone(),
            bootstrap_token,
            package_path: installation.package_path,
            maximum_active_pi_sessions: None,
            default_model,
            job_wrapper,
            schema_dir: installation.schema_dir,
        },
        Rc::new(adapter),
        (connected, inbox),
    )
    .map_err(|error| error.to_string())?;
    daemon.subscribe(Rc::new(|event: &Value| {
        if let Some(line) = operator_line(event) {
            log(&line);
        }
    }));
    daemon.start().map_err(|error| error.to_string())?;
    let server = LocalApiServer::new(Rc::clone(&daemon));
    if let Err(error) = server.listen("127.0.0.1", port).await {
        daemon.stop();
        return Err(error.to_string());
    }
    log(&format!(
        "noshd listening on 127.0.0.1:{port} (data {})",
        data_directory.display()
    ));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminated() => {}
        () = server.shutdown_requested() => {}
    }
    log("noshd shutting down");
    let _ = server.close().await;
    daemon.stop();
    Ok(())
}

/// SIGTERM; Windows has none.
async fn terminated() {
    #[cfg(unix)]
    if let Ok(mut signal) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        signal.recv().await;
        return;
    }
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_arguments_as_main_ts_does() {
        let args = arguments(&[
            "--port=0".into(),
            "--data-dir=C:\\a=b".into(),
            "--port=5".into(),
            "--flag".into(),
        ]);
        assert_eq!(args["--port"], "5");
        assert_eq!(args["--data-dir"], "C:\\a");
        assert_eq!(args["--flag"], "");
    }

    #[test]
    fn finds_the_installation_above_the_executable() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("pkg");
        let executable = package.join("bin").join("nosh.exe");
        let host_script = package.join("sidecar").join("pi-host").join("host.mjs");
        for file in [&executable, &host_script] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "").unwrap();
        }
        let installation = Installation::find_from(&executable).unwrap();
        assert_eq!(installation.package_path, package.join("pi-package"));
        assert_eq!(installation.host_script, host_script);
        assert_eq!(
            installation.schema_dir,
            package.join("conformance").join("schemas").join("json")
        );
        std::fs::remove_file(&host_script).unwrap();
        assert_eq!(
            Installation::find_from(&executable).err().unwrap(),
            format!(
                "No NOSH installation (sidecar/pi-host/host.mjs) found above {}",
                executable.display()
            )
        );
    }

    #[test]
    fn logs_lifecycle_and_failure_events_only() {
        let event = json!({
            "type": "mission.stopped",
            "scope": { "agentId": null, "directionId": "dir_1", "missionId": "mis_1", "projectId": "prj_1" },
            "payload": { "message": "x".repeat(400) },
        });
        assert_eq!(
            operator_line(&event).unwrap(),
            format!(
                "mission.stopped missionId=mis_1 directionId=dir_1 :: {}",
                "x".repeat(300)
            )
        );
        let quiet = json!({ "type": "project.registered", "scope": {}, "payload": {} });
        assert_eq!(operator_line(&quiet), None);
        let verbose = json!({ "type": "agent.failed", "scope": {}, "payload": {} });
        assert_eq!(operator_line(&verbose), None);
        let bare = json!({ "type": "recovery.report", "scope": { "projectId": "prj_1" }, "payload": { "message": 5 } });
        assert_eq!(operator_line(&bare).unwrap(), "recovery.report ");
    }
}
