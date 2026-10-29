//! Black-box conformance of the `nosh` CLI. One command sequence runs through `nosh` and, when the
//! TypeScript tree is built (`NOSH_TS_ROOT`, else this worktree's parent), through
//! `node apps/cli/dist/main.js`, each with its own state root (LOCALAPPDATA, APPDATA and the home
//! Pi uses) and its own daemon port. Exit codes, stdout and stderr must agree once the state
//! directory, IDs, times and ports are masked.
//!
//! Deliberate differences are checked on the Rust side and left out of the comparison: the first two
//! `update` lines (DECISIONS 14) and TypeScript's Bun rows in `setup` and `doctor` (DECISIONS 13).
//! `setup` runs on a config already written for the test's port, which it never overwrites. The
//! daemon log is compared only before a daemon has written it. A running Job is the active work
//! behind `stop --if-idle`'s exit 2. No model account is connected, and Pi stays offline
//! (`PI_OFFLINE`), so its model catalogs come from its own package on both sides.
//!
//! Needs `npm ci` in `sidecar/pi-host`.

use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

mod common;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Implementation {
    Rust,
    TypeScript,
}

fn ts_root() -> PathBuf {
    std::env::var_os("NOSH_TS_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        Into::into,
    )
}

/// Whether the TypeScript CLI and daemon builds are present. CI always builds them, so there a
/// missing build fails instead of silently skipping the comparison.
fn ts_built() -> bool {
    let files =
        ["apps/cli/dist/main.js", "apps/noshd/dist/main.js"].map(|file| ts_root().join(file));
    let built = files.iter().all(|file| file.is_file());
    assert!(
        built || std::env::var_os("CI").is_none(),
        "CI builds the TypeScript reference, but {} is missing",
        ts_root().display()
    );
    if !built {
        eprintln!(
            "TypeScript build missing under {}; skipped",
            ts_root().display()
        );
    }
    built
}

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-z]+)_[0-9a-f]{32}\b").unwrap());
/// ISO times, and backup IDs' times with `-` for `:`.
static TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}[:-]\d{2}[:-]\d{2}(\.\d+)?Z").unwrap());

/// One implementation's CLI with its own state root and daemon port.
struct Side {
    implementation: Implementation,
    dir: tempfile::TempDir,
    port: u16,
    /// Forms of the state directory as outputs spell it, longest first.
    paths: Vec<String>,
    ids: Vec<String>,
}

/// What a command printed, masked.
#[derive(Debug, PartialEq)]
struct Outcome {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Side {
    fn new(implementation: Implementation) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let canonical = std::fs::canonicalize(dir.path())
            .unwrap()
            .display()
            .to_string();
        let canonical = canonical
            .strip_prefix(r"\\?\")
            .unwrap_or(&canonical)
            .to_owned();
        let mut paths = vec![dir.path().display().to_string(), canonical];
        for form in paths.clone() {
            paths.push(form.replace('\\', "/"));
            paths.push(form.replace('\\', "\\\\"));
        }
        paths.sort_by_key(|form| std::cmp::Reverse(form.len()));
        paths.dedup();
        let side = Self {
            implementation,
            dir,
            port,
            paths,
            ids: Vec::new(),
        };
        std::fs::create_dir_all(side.root()).unwrap();
        side
    }

    /// The NOSH state root.
    fn root(&self) -> PathBuf {
        self.dir.path().join("NOSH")
    }

    fn data(&self) -> PathBuf {
        self.root().join("data")
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// The config this side starts from, on its own port.
    fn config(&self) -> Value {
        json!({ "dataDirectory": self.data(), "port": self.port, "bootstrapToken": "conformance", "currentProjectId": null })
    }

    fn write_config(&self, text: &str) {
        std::fs::write(self.root().join("config.json"), text).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = match self.implementation {
            Implementation::Rust => Command::new(env!("CARGO_BIN_EXE_nosh")),
            Implementation::TypeScript => {
                let mut command = Command::new("node");
                command.arg(ts_root().join("apps/cli/dist/main.js"));
                command
            }
        };
        command
            .args(args)
            .env("LOCALAPPDATA", self.dir.path())
            .env("APPDATA", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .env("HOME", self.dir.path())
            .env("PI_OFFLINE", "1")
            .env_remove("NOSH_DEFAULT_MODEL")
            .env_remove("NOSH_NODE")
            .stdin(Stdio::null());
        command
    }

    /// `POST /api<path>` to this side's daemon, trusted as the CLI is (loopback, no credentials).
    fn post(&self, path: &str, body: &Value) -> Value {
        let url = format!("http://127.0.0.1:{}/api{path}", self.port);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let response = client.post(url).json(body).send().await.unwrap();
            let status = response.status();
            let reply: Value = response.json().await.unwrap();
            assert!(
                status.is_success(),
                "{:?} POST {path}: {reply}",
                self.implementation
            );
            reply
        })
    }

    /// Runs `job list` until its output contains `state`.
    fn await_job(&mut self, state: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !self.run(&["job", "list"]).out.contains(state) {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?} Job never {state:?}",
                self.implementation
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    fn run(&mut self, args: &[&str]) -> Outcome {
        let output =
            common::output(&mut self.command(args)).unwrap_or_else(|error| panic!("{error}"));
        Outcome {
            code: output.status.code(),
            out: self.mask(&String::from_utf8_lossy(&output.stdout)),
            err: self.mask(&String::from_utf8_lossy(&output.stderr)),
        }
    }

    /// Masks the state directory, IDs (numbered by first appearance), times and the port.
    fn mask(&mut self, text: &str) -> String {
        let mut text = text.to_owned();
        for form in &self.paths {
            text = text.replace(form.as_str(), "<state>");
        }
        text = text.replace(&format!("127.0.0.1:{}", self.port), "127.0.0.1:<port>");
        text = text.replace(&format!("\"port\":{}", self.port), "\"port\":<port>");
        let ids = &mut self.ids;
        let text = ID.replace_all(&text, |found: &regex::Captures| {
            let index = ids
                .iter()
                .position(|id| *id == found[0])
                .unwrap_or_else(|| {
                    ids.push(found[0].to_owned());
                    ids.len() - 1
                });
            format!("<{}#{index}>", &found[1])
        });
        TIME.replace_all(&text, "<time>").into_owned()
    }
}

/// A repository with one commit at `repo`.
fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    std::fs::write(repo.join("README.md"), "conformance\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "test@nosh.invalid"],
        &["config", "user.name", "NOSH test"],
    ] {
        git(repo, args);
    }
    commit_all(repo);
}

impl Drop for Side {
    /// Stops this side's daemon through its own CLI, from a known good config.
    fn drop(&mut self) {
        if !self.root().join("config.json").exists() {
            return;
        }
        self.write_config(&self.config().to_string());
        let stopped = common::output(&mut self.command(&["stop"]))
            .and_then(|_| common::output(&mut self.command(&["status"])));
        if !std::thread::panicking() {
            let status = stopped.unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(
                String::from_utf8_lossy(&status.stdout),
                "stopped\n",
                "{:?} daemon left running",
                self.implementation
            );
        }
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_all(repo: &Path) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "conformance"]);
}

/// Both sides, Rust first.
struct Sides([Side; 2]);

impl Sides {
    /// Runs `args` on both sides and requires the same masked outcome, which it returns.
    fn same(&mut self, args: &[&str]) -> Outcome {
        let [rust, typescript] = &mut self.0;
        let (rust, typescript) = (rust.run(args), typescript.run(args));
        assert_eq!(
            rust, typescript,
            "nosh {args:?}: Rust (left) and TypeScript differ"
        );
        rust
    }

    /// `same`, once TypeScript's Bun rows are dropped (DECISIONS 13): the doctor's
    /// `PASS|FAIL\tBun (terminal UI)\t…` and setup's `✓ Bun …` or `✗ Bun …` with its `→` fix line.
    fn same_but_bun(&mut self, args: &[&str]) -> Outcome {
        let [rust, typescript] = &mut self.0;
        let (rust, mut typescript) = (rust.run(args), typescript.run(args));
        let mut kept = String::new();
        let mut dropping = false;
        for line in typescript.out.split_inclusive('\n') {
            let fix = dropping && line.starts_with("      → ");
            dropping = line.contains("\tBun (terminal UI)\t")
                || line.contains(" Bun >= 1.3 (terminal UI)");
            if !dropping && !fix {
                kept.push_str(line);
            }
        }
        typescript.out = kept;
        assert_eq!(
            rust, typescript,
            "nosh {args:?}: Rust (left) and TypeScript differ"
        );
        assert!(!rust.out.contains("Bun"), "{}", rust.out);
        rust
    }

    /// `same` with each side's own arguments (its own paths and IDs).
    fn same_each(&mut self, args: impl Fn(&Side) -> Vec<String>) -> Outcome {
        let [rust, typescript] = &mut self.0;
        let (rust_args, typescript_args) = (args(rust), args(typescript));
        let run = |side: &mut Side, args: &[String]| {
            side.run(&args.iter().map(String::as_str).collect::<Vec<_>>())
        };
        let (rust, typescript) = (run(rust, &rust_args), run(typescript, &typescript_args));
        assert_eq!(
            rust, typescript,
            "nosh {rust_args:?}: Rust (left) and TypeScript differ"
        );
        rust
    }

    fn each(&self, mut action: impl FnMut(&Side)) {
        self.0.iter().for_each(&mut action);
    }

    fn each_mut(&mut self, action: impl FnMut(&mut Side)) {
        self.0.iter_mut().for_each(action);
    }

    fn rust(&mut self) -> &mut Side {
        &mut self.0[0]
    }
}

#[test]
fn cli_commands_agree_with_typescript() {
    if !ts_built() {
        return;
    }
    let mut sides = Sides([
        Side::new(Implementation::Rust),
        Side::new(Implementation::TypeScript),
    ]);

    // No config.
    assert!(sides.same(&["help"]).out.starts_with("nosh setup"));
    sides.same(&[]);
    for args in [
        &["status"][..],
        &["logs"],
        &["model"],
        &["doctor"],
        &["logout"],
    ] {
        assert_eq!(sides.same(args).code, Some(1), "{args:?}");
    }
    assert_eq!(sides.same(&["stop", "--if-idle"]).code, Some(3));
    let [rust, typescript] = &mut sides.0;
    let (rust_update, typescript_update) = (rust.run(&["update"]), typescript.run(&["update"]));
    let root = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .unwrap()
        .display()
        .to_string();
    let root = root.strip_prefix(r"\\?\").unwrap_or(&root);
    assert_eq!(
        rust_update.out,
        format!(
            "This NOSH runs from {root}{}.\nSource checkout: git pull && cargo build --release, then nosh stop and nosh start.\nnpm install: npm install -g nosh-harness@latest, then nosh update.\n",
            std::path::MAIN_SEPARATOR
        )
    );
    assert_eq!(
        rust_update.out.lines().nth(2),
        typescript_update.out.lines().nth(2)
    );
    assert_eq!(
        (rust_update.code, &rust_update.err),
        (typescript_update.code, &typescript_update.err)
    );

    // A config on an unused port: no daemon.
    sides.each(|side| side.write_config(&side.config().to_string()));
    for args in [
        &["status"][..],
        &["stop"],
        &["logs"],
        &["model"],
        &["model", "show"],
        &["model", "list"],
        &["model", "set"],
        &["model", "frob"],
        &["model", "set", "nope"],
        &["model", "set", "a/b:foo"],
        &["model", "set", "a/b:LOW"],
        &["project", "list"],
        &["project", "open"],
        &["mission", "list"],
        &["mission", "status"],
        &["job", "list"],
        &["backup"],
        &["backup", "prj_missing"],
        &["backup", "restore", "prj_missing"],
        &["backup", "restore", "prj_missing", "bak"],
        &["frob", "--x=1"],
        &["logout", "nosuch"],
        &["login"],
        &["open"],
        &["tui"],
    ] {
        sides.same(args);
    }
    assert_eq!(sides.same(&["stop", "--if-idle"]).code, Some(3));
    assert_eq!(sides.same(&["--if-idle", "stop"]).code, Some(3));
    let listed = sides.same(&["login", "--list"]);
    assert!(listed.out.contains("anthropic"), "{}", listed.out);

    assert_eq!(sides.same_but_bun(&["doctor"]).code, Some(1));

    // Config edge cases: a BOM and unknown keys survive, and the rewrite is canonical.
    sides.each(|side| {
        let mut config = side.config();
        config["zeta"] = json!({ "b": 1, "a": [2] });
        side.write_config(&format!("\u{feff}{config}"));
    });
    sides.same(&["model", "set", "p/m:high"]);
    let [rust, typescript] = &mut sides.0;
    let saved = |side: &mut Side| {
        let text = std::fs::read_to_string(side.root().join("config.json")).unwrap();
        side.mask(&text)
    };
    assert_eq!(saved(rust), saved(typescript));
    for text in ["{nope", "", "{\"port\":1,}", "[1,", "\"x"] {
        sides.each(|side| side.write_config(text));
        assert_eq!(sides.same(&["status"]).code, Some(1), "{text}");
    }
    sides.each(|side| side.write_config(&side.config().to_string()));

    // The daemon log as written.
    sides.each(|side| {
        std::fs::create_dir_all(side.data().join("logs")).unwrap();
        let lines: String = (1..=230).map(|line| format!("line {line}\r\n")).collect();
        std::fs::write(side.data().join("logs").join("noshd.log"), lines).unwrap();
    });
    sides.same(&["logs"]);

    // Opening Projects without a daemon.
    sides.each(|side| {
        std::fs::create_dir_all(side.dir.path().join("plain")).unwrap();
        std::fs::write(side.dir.path().join("file.txt"), "x").unwrap();
    });
    for path in ["missing", "plain", "file.txt"] {
        let open = sides.same_each(|side| project_open(&side.dir.path().join(path)));
        assert_eq!(open.code, Some(1), "project open {path}");
    }
    sides.each(|side| init_repo(&side.repo()));
    let opened = sides.same_each(|side| project_open(&side.repo()));
    assert!(
        opened.out.starts_with("Opened <prj#0>\nPaper: <state>"),
        "{opened:?}"
    );
    let backup = |side: &Side| vec!["backup".to_owned(), side.ids[0].clone()];
    sides.same(&["project", "list"]);
    sides.same(&["mission", "list"]);
    sides.same_each(backup);

    // With a daemon, which setup starts. No model account is connected, so setup is not finished,
    // and it never reaches the default model: `--model` is not saved. Setup keeps the config and
    // so the test's port; a rewrite would name 4321, the user's daemon. Without Node, setup stops
    // before Pi or a daemon, so a rewrite is caught first there.
    let config_text =
        |side: &Side| std::fs::read_to_string(side.root().join("config.json")).unwrap();
    let configs: Vec<String> = sides.0.iter().map(config_text).collect();
    let rust = sides.rust();
    let mut without_node = rust.command(&["setup", "--yes"]);
    without_node.env("NOSH_NODE", rust.dir.path().join("missing-node.exe"));
    let stopped = common::output(&mut without_node).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(config_text(rust), configs[0], "setup rewrote config.json");
    assert_eq!(
        (
            stopped.status.code(),
            String::from_utf8_lossy(&stopped.stderr)
        ),
        (
            Some(1),
            "nosh: fix the prerequisites above, then run `nosh setup` again\n".into()
        )
    );
    let set_up = sides.same_but_bun(&["setup", "--yes"]);
    assert!(
        set_up
            .out
            .contains("noshd started at http://127.0.0.1:<port>\n\nSetup is not finished"),
        "{set_up:?}"
    );
    assert_eq!(set_up.code, Some(0));
    assert_eq!(sides.0.iter().map(config_text).collect::<Vec<_>>(), configs);
    assert!(
        sides
            .same(&["start"])
            .out
            .starts_with("noshd already running")
    );
    sides.same_but_bun(&["setup", "--yes", "--model=p/m"]);
    sides.each(|side| {
        let config = std::fs::read_to_string(side.root().join("config.json")).unwrap();
        assert!(!config.contains("defaultModel"), "{config}");
    });
    assert_eq!(
        sides.same(&["status"]).out,
        "running http://127.0.0.1:<port>\n"
    );
    // A Project opened now is registered through the daemon and becomes the current one.
    sides.each(|side| init_repo(&side.dir.path().join("second")));
    let opened = sides.same_each(|side| project_open(&side.dir.path().join("second")));
    assert!(opened.out.starts_with("Opened <prj#1>"), "{opened:?}");
    for args in [
        &["project", "list"][..],
        &["mission", "list"],
        &["mission", "status", "msn_nope"],
        &["job", "list"],
        &["model", "list"],
        &["backup", "prj_missing"],
    ] {
        sides.same(args);
    }
    assert_eq!(sides.same_but_bun(&["doctor"]).code, Some(1));
    for backup_id in ["bad..id", "<id>-2026-01-01T00-00-00.000Z", "<id>"] {
        let restore = sides.same_each(|side| {
            let id = side.ids[0].clone();
            let backup_id = backup_id.replace("<id>", &id);
            vec!["backup".into(), "restore".into(), id, backup_id]
        });
        assert_eq!(restore.code, Some(1), "backup restore {backup_id}");
    }
    // The opened repository has NOSH's files uncommitted until now.
    sides.same_each(backup);
    sides.each(|side| commit_all(&side.repo()));
    let written = sides.same_each(backup);
    assert!(
        written.out.starts_with("Backup written to <state>"),
        "{written:?}"
    );
    sides.same(&["model", "set", "p/m"]);

    // A running Job is active work: `stop --if-idle` exits 2 and stops nothing, and backups wait.
    sides.each(|side| {
        side.post("/jobs", &job_spec(&side.ids[0], &side.repo()));
    });
    sides.each_mut(|side| side.await_job("\trunning\t"));
    let busy = sides.same(&["stop", "--if-idle"]);
    assert_eq!((busy.code, busy.out.as_str()), (Some(2), ""), "{busy:?}");
    assert!(
        busy.err
            .starts_with("noshd has active work; finish or stop it first:\n  <prj#0>: job <job#")
            && busy.err.ends_with(":running\n"),
        "{busy:?}"
    );
    assert_eq!(
        sides.same(&["status"]).out,
        "running http://127.0.0.1:<port>\n"
    );
    let refused = sides.same_each(backup);
    assert!(
        refused.err.contains("active Project state (job <job#"),
        "{refused:?}"
    );
    sides.same(&["job", "list"]);
    sides.each(|side| {
        let body = json!({ "projectId": side.ids[0], "idempotencyKey": "conformance-cancel" });
        side.post(&format!("/jobs/{JOB}/cancel"), &body);
    });
    sides.each_mut(|side| side.await_job("\tcancelled\t"));
    sides.same(&["job", "list"]);
    assert_eq!(sides.same(&["stop", "--if-idle"]).code, Some(0));
    assert_eq!(sides.same(&["stop", "--if-idle"]).code, Some(3));
    sides.same(&["status"]);

    // A default model the daemon refuses at startup.
    sides.each(|side| {
        let mut config = side.config();
        config["defaultModel"] = json!("nope");
        side.write_config(&config.to_string());
    });
    let refused = sides.same(&["start"]);
    assert!(
        refused.err.contains("noshd exited during startup"),
        "{refused:?}"
    );
    sides.same(&["status"]);
    assert_eq!(sides.rust().run(&["status"]).out, "stopped\n");
}

fn project_open(path: &Path) -> Vec<String> {
    vec!["project".into(), "open".into(), path.display().to_string()]
}

const JOB: &str = "job_0123456789abcdef0123456789abcdef";

/// A native Job in `repo` that runs for a minute unless it is cancelled.
fn job_spec(project_id: &str, repo: &Path) -> Value {
    let hash = format!("sha256:{}", "0".repeat(64));
    json!({
        "jobId": JOB, "projectId": project_id, "missionId": null, "directionId": null,
        "autoresearchId": null, "runId": "run_0123456789abcdef0123456789abcdef",
        "experimentId": null, "commitSha": "0".repeat(40), "workingDirectory": repo,
        "runner": "native", "distribution": null,
        "command": ["node", "-e", "setTimeout(() => {}, 60000)"], "checkpointCommand": null,
        "environmentLockHash": hash, "evaluationContractHash": hash, "timeoutSeconds": 60,
        "usesGpu": false
    })
}
