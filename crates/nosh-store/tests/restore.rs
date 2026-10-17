//! Restore tests: the TypeScript restore tests ported (exact restores, refusals, scheduled plans,
//! and rollback at every phase), then restores across implementations.

use nosh_store::backup::{RegisteredProject, create_project_backup};
use nosh_store::restore::{
    RestoreOptions, apply_scheduled_restores, restore_backup, schedule_project_restore,
};
use nosh_store::{Error, HostRegistry};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROJECT: &str = "prj_backup_contract";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_fails(cwd: &Path, args: &[&str]) -> bool {
    !Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", message]);
}

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    database: PathBuf,
    project: RegisteredProject,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@nosh.invalid"]);
    git(&repo, &["config", "user.name", "NOSH test"]);
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let database = root.join("project.sqlite");
    drop(nosh_store::open_database(&database).unwrap());
    let project = RegisteredProject {
        project_id: PROJECT.into(),
        repository_root: repo.to_string_lossy().into(),
        database_path: database.to_string_lossy().into(),
        registered_at: "2026-01-01T00:00:00.000Z".into(),
    };
    Workspace {
        _dir: dir,
        root,
        repo,
        data,
        database,
        project,
    }
}

fn add_units(repo: &Path) {
    for unit in ["contracts", "events", "sessions"] {
        std::fs::create_dir_all(repo.join(".nosh").join(unit)).unwrap();
    }
    std::fs::create_dir_all(repo.join("docs/figures")).unwrap();
    std::fs::write(repo.join(".nosh/project.json"), "{\"marker\":\"backup\"}\n").unwrap();
    std::fs::write(repo.join(".nosh/schema-lock.json"), "{}\n").unwrap();
    std::fs::write(repo.join("docs/paper.md"), "# Backup paper\n").unwrap();
    std::fs::write(repo.join("docs/figures/figure.txt"), "figure\n").unwrap();
}

fn add_job(data: &Path, name: &str, project_id: &str) {
    std::fs::create_dir_all(data.join("jobs").join(name)).unwrap();
    std::fs::write(
        data.join("jobs").join(name).join("job.json"),
        json!({ "projectId": project_id }).to_string(),
    )
    .unwrap();
}

fn set_sequence(database: &Path, value: i64) {
    let connection = nosh_store::open_database(database).unwrap();
    connection.execute("INSERT INTO project_sequences (project_id, last_sequence) VALUES (?1, ?2) ON CONFLICT(project_id) DO UPDATE SET last_sequence = ?2", rusqlite::params![PROJECT, value]).unwrap();
}

fn sequence(database: &Path) -> i64 {
    nosh_store::open_database(database)
        .unwrap()
        .query_row(
            "SELECT last_sequence FROM project_sequences WHERE project_id = ?",
            [PROJECT],
            |row| row.get(0),
        )
        .unwrap()
}

fn restore(w: &Workspace, backup: &str, options: RestoreOptions<'_>) -> nosh_store::Result<()> {
    restore_backup(
        &w.repo,
        &w.data,
        Path::new(backup),
        PROJECT,
        &w.database,
        true,
        options,
    )
    .map(|_| ())
}

fn assert_no_restore_debris(root: &Path) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".nosh-restore-")
                && !name.starts_with(".nosh-rollback-")
                && !name.starts_with(".nosh-restore-db-"),
            "debris {name} in {}",
            root.display()
        );
        if entry.file_type().unwrap().is_dir() {
            assert_no_restore_debris(&entry.path());
        }
    }
}

/// Port of "restores the backed-up repository, database, artifacts, paper, and matching jobs exactly".
#[test]
fn restores_repository_database_units_and_jobs_exactly() {
    let w = workspace();
    add_units(&w.repo);
    std::fs::write(w.repo.join("tracked.txt"), "before\n").unwrap();
    commit_all(&w.repo, "fixture");
    let (head, reference) = (
        git(&w.repo, &["rev-parse", "HEAD"]),
        git(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]),
    );
    set_sequence(&w.database, 7);
    add_job(&w.data, "job-match", PROJECT);
    add_job(&w.data, "job-other", "prj_other");
    std::fs::write(w.data.join("jobs/job-match/marker.txt"), "before\n").unwrap();
    std::fs::write(w.data.join("jobs/job-other/marker.txt"), "keep\n").unwrap();
    let summary = create_project_backup(&w.data, &w.project).unwrap();

    std::fs::write(w.repo.join("docs/paper.md"), "# Changed\n").unwrap();
    std::fs::write(
        w.repo.join(".nosh/project.json"),
        "{\"marker\":\"changed\"}\n",
    )
    .unwrap();
    commit_all(&w.repo, "changed");
    set_sequence(&w.database, 99);
    std::fs::write(w.data.join("jobs/job-match/marker.txt"), "changed\n").unwrap();
    restore(&w, &summary.path, RestoreOptions::default()).unwrap();

    assert_eq!(
        git(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]),
        reference
    );
    assert_eq!(git(&w.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&w.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(w.repo.join("docs/paper.md")).unwrap(),
        "# Backup paper\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.repo.join(".nosh/project.json")).unwrap(),
        "{\"marker\":\"backup\"}\n"
    );
    assert_eq!(sequence(&w.database), 7);
    assert_eq!(
        std::fs::read_to_string(w.data.join("jobs/job-match/marker.txt")).unwrap(),
        "before\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.data.join("jobs/job-other/marker.txt")).unwrap(),
        "keep\n"
    );
    assert_no_restore_debris(&w.root);
}

/// Ports of "restores a detached unique HEAD commit" and "restores an unborn HEAD over a later commit".
#[test]
fn restores_detached_and_unborn_heads() {
    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "base\n").unwrap();
    commit_all(&w.repo, "base");
    let base = git(&w.repo, &["rev-parse", "HEAD"]);
    git(&w.repo, &["checkout", "-q", "--detach", &base]);
    std::fs::write(w.repo.join("tracked.txt"), "unique\n").unwrap();
    commit_all(&w.repo, "unique");
    let unique = git(&w.repo, &["rev-parse", "HEAD"]);
    let summary = create_project_backup(&w.data, &w.project).unwrap();
    std::fs::write(w.repo.join("tracked.txt"), "later\n").unwrap();
    commit_all(&w.repo, "later");
    restore(&w, &summary.path, RestoreOptions::default()).unwrap();
    assert!(git_fails(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]));
    assert_eq!(git(&w.repo, &["rev-parse", "HEAD"]), unique);
    assert_eq!(
        std::fs::read_to_string(w.repo.join("tracked.txt")).unwrap(),
        "unique\n"
    );

    let w = workspace();
    let unborn = create_project_backup(&w.data, &w.project).unwrap();
    std::fs::write(w.repo.join("later.txt"), "later\n").unwrap();
    commit_all(&w.repo, "later");
    restore(&w, &unborn.path, RestoreOptions::default()).unwrap();
    assert_eq!(
        git(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]),
        "refs/heads/main"
    );
    assert!(git_fails(&w.repo, &["rev-parse", "--verify", "HEAD"]));
    assert!(!w.repo.join("later.txt").exists());
    assert_eq!(
        git(&w.repo, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

/// Port of "rejects identity mismatch, daemon-running restore, and dirty target before import".
#[test]
fn refuses_before_changing_anything() {
    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "one\n").unwrap();
    commit_all(&w.repo, "fixture");
    let summary = create_project_backup(&w.data, &w.project).unwrap();
    let path = Path::new(&summary.path);
    let error = |id: &str, stopped: bool| {
        restore_backup(
            &w.repo,
            &w.data,
            path,
            id,
            &w.database,
            stopped,
            RestoreOptions::default(),
        )
        .unwrap_err()
        .to_string()
    };
    assert_eq!(
        error("prj_other", true),
        "Backup belongs to a different Project"
    );
    assert_eq!(
        error(PROJECT, false),
        "Refusing restore while noshd is running; stop the daemon first"
    );
    std::fs::write(w.repo.join("dirty.txt"), "dirty\n").unwrap();
    assert_eq!(
        error(PROJECT, true),
        "Project repository must be exactly clean, including untracked files"
    );
    std::fs::remove_file(w.repo.join("dirty.txt")).unwrap();
    std::fs::write(path.join("manifest.json"), "{}\n").unwrap();
    assert!(schedule_project_restore(&w.data, &w.project, &summary.backup_id).is_err());
}

/// Port of "rolls back every reversible restore phase".
#[test]
fn rolls_back_every_reversible_phase() {
    for phase in [
        "after-refs",
        "after-head-tree",
        "after-filesystem-swap",
        "before-success",
        "on-commit",
    ] {
        let w = workspace();
        add_units(&w.repo);
        std::fs::write(w.repo.join("tracked.txt"), "before\n").unwrap();
        commit_all(&w.repo, "fixture");
        add_job(&w.data, "job-match", PROJECT);
        add_job(&w.data, "job-other", "prj_other");
        set_sequence(&w.database, 7);
        let summary = create_project_backup(&w.data, &w.project).unwrap();

        std::fs::write(w.repo.join("tracked.txt"), "after\n").unwrap();
        std::fs::write(w.repo.join("docs/paper.md"), "# Changed paper\n").unwrap();
        commit_all(&w.repo, "after");
        set_sequence(&w.database, 99);
        std::fs::write(w.data.join("jobs/job-match/marker.txt"), "after\n").unwrap();
        let before = (
            git(&w.repo, &["rev-parse", "HEAD"]),
            git(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]),
            git(
                &w.repo,
                &["for-each-ref", "--format=%(objectname) %(refname)"],
            ),
            std::fs::read(&w.database).unwrap(),
        );

        let injected = || Err(Error::Rejected("injected".into()));
        let options = if phase == "on-commit" {
            RestoreOptions {
                on_phase: None,
                on_commit: Some(Box::new(injected)),
            }
        } else {
            RestoreOptions {
                on_phase: Some(Box::new(
                    move |current| if current == phase { injected() } else { Ok(()) },
                )),
                on_commit: None,
            }
        };
        assert_eq!(
            restore(&w, &summary.path, options).unwrap_err().to_string(),
            "injected",
            "{phase}"
        );
        let after = (
            git(&w.repo, &["rev-parse", "HEAD"]),
            git(&w.repo, &["symbolic-ref", "--quiet", "HEAD"]),
            git(
                &w.repo,
                &["for-each-ref", "--format=%(objectname) %(refname)"],
            ),
            std::fs::read(&w.database).unwrap(),
        );
        assert_eq!(
            after, before,
            "{phase}: Git state and database are rolled back"
        );
        assert_eq!(
            git(&w.repo, &["status", "--porcelain", "--untracked-files=all"]),
            "",
            "{phase}"
        );
        assert_eq!(
            std::fs::read_to_string(w.repo.join("tracked.txt")).unwrap(),
            "after\n",
            "{phase}"
        );
        assert_eq!(
            std::fs::read_to_string(w.repo.join("docs/paper.md")).unwrap(),
            "# Changed paper\n",
            "{phase}"
        );
        assert_eq!(
            std::fs::read_to_string(w.data.join("jobs/job-match/marker.txt")).unwrap(),
            "after\n",
            "{phase}"
        );
        assert!(w.data.join("jobs/job-other").exists(), "{phase}");
        assert_no_restore_debris(&w.root);
    }
}

/// Port of "removes a committed restore plan without replaying its restore", plus a pending plan.
#[test]
fn applies_pending_plans_and_drops_committed_ones() {
    let w = workspace();
    std::fs::write(w.repo.join("tracked.txt"), "before\n").unwrap();
    commit_all(&w.repo, "before");
    let summary = create_project_backup(&w.data, &w.project).unwrap();
    HostRegistry::open(&w.data.join("host.sqlite"))
        .unwrap()
        .register(PROJECT, &w.repo, &w.database)
        .unwrap();
    let registered = registered(&w);
    schedule_project_restore(&w.data, &registered, &summary.backup_id).unwrap();
    let plan_path = w.data.join("restore-plans").join(format!("{PROJECT}.json"));
    let mut plan: Value =
        serde_json::from_str(&std::fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["state"] = json!("committed");
    std::fs::write(&plan_path, format!("{plan}\n")).unwrap();
    std::fs::write(w.repo.join("tracked.txt"), "after\n").unwrap();
    commit_all(&w.repo, "after");
    let changed = git(&w.repo, &["rev-parse", "HEAD"]);
    assert_eq!(apply_scheduled_restores(&w.data).unwrap(), 1);
    assert!(!plan_path.exists());
    assert_eq!(
        git(&w.repo, &["rev-parse", "HEAD"]),
        changed,
        "a committed plan is not replayed"
    );

    schedule_project_restore(&w.data, &registered, &summary.backup_id).unwrap();
    assert_eq!(apply_scheduled_restores(&w.data).unwrap(), 1);
    assert!(!plan_path.exists());
    assert_eq!(
        std::fs::read_to_string(w.repo.join("tracked.txt")).unwrap(),
        "before\n",
        "a pending plan restores"
    );
}

fn registered(w: &Workspace) -> RegisteredProject {
    let row = HostRegistry::open(&w.data.join("host.sqlite"))
        .unwrap()
        .list()
        .unwrap()
        .into_iter()
        .find(|row| row["projectId"] == PROJECT)
        .unwrap();
    serde_json::from_value(row).unwrap()
}

// ---- Across implementations ----

fn typescript(calls: Value) -> Vec<Value> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let request = json!({ "module": "packages/persistence/dist/index.js", "calls": calls });
    let mut child = Command::new("node")
        .arg(repo_root.join("scripts/ts-call.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "ts-call.mjs failed (is the TypeScript tree built?):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    for result in &results {
        assert!(
            result.get("ok").is_some(),
            "TypeScript call failed: {result}"
        );
    }
    results
}

/// The state a restore must reproduce: Git HEAD and refs, the database, and the research units.
fn snapshot(w: &Workspace) -> (String, String, i64, String) {
    (
        git(&w.repo, &["rev-parse", "HEAD"]),
        git(
            &w.repo,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        ),
        sequence(&w.database),
        std::fs::read_to_string(w.repo.join("docs/paper.md")).unwrap(),
    )
}

fn diverge(w: &Workspace) {
    std::fs::write(w.repo.join("docs/paper.md"), "# Diverged\n").unwrap();
    commit_all(&w.repo, "diverged");
    git(&w.repo, &["branch", "-D", "feature"]);
    set_sequence(&w.database, 99);
}

fn populated() -> Workspace {
    let w = workspace();
    add_units(&w.repo);
    std::fs::write(w.repo.join("tracked.txt"), "before\n").unwrap();
    commit_all(&w.repo, "fixture");
    git(&w.repo, &["branch", "feature"]);
    set_sequence(&w.database, 7);
    add_job(&w.data, "job-match", PROJECT);
    w
}

#[test]
fn typescript_restores_a_rust_backup() {
    let w = populated();
    let expected = snapshot(&w);
    let ours = create_project_backup(&w.data, &w.project).unwrap();
    diverge(&w);
    typescript(
        json!([{ "method": "restoreBackup", "args": [w.repo, w.data, ours.path, PROJECT, w.database, true] }]),
    );
    assert_eq!(snapshot(&w), expected);
    assert_no_restore_debris(&w.root);
}

#[test]
fn rust_restores_a_typescript_backup() {
    let w = populated();
    let expected = snapshot(&w);
    let theirs =
        typescript(json!([{ "method": "createProjectBackup", "args": [w.data, w.project] }]));
    diverge(&w);
    restore(
        &w,
        theirs[0]["ok"]["path"].as_str().unwrap(),
        RestoreOptions::default(),
    )
    .unwrap();
    assert_eq!(snapshot(&w), expected);
    assert_no_restore_debris(&w.root);
}

#[test]
fn plans_scheduled_by_one_implementation_are_applied_by_the_other() {
    let w = populated();
    let expected = snapshot(&w);
    HostRegistry::open(&w.data.join("host.sqlite"))
        .unwrap()
        .register(PROJECT, &w.repo, &w.database)
        .unwrap();
    let project = registered(&w);
    let backup = create_project_backup(&w.data, &project).unwrap();

    diverge(&w);
    schedule_project_restore(&w.data, &project, &backup.backup_id).unwrap();
    let applied = typescript(json!([{ "method": "applyScheduledRestores", "args": [w.data] }]));
    assert_eq!(applied[0]["ok"], 1);
    assert_eq!(snapshot(&w), expected, "TypeScript applies a Rust plan");

    git(&w.repo, &["branch", "feature2"]);
    set_sequence(&w.database, 55);
    let changed = snapshot(&w);
    assert_ne!(changed, expected);
    typescript(
        json!([{ "method": "scheduleProjectRestore", "args": [w.data, project, backup.backup_id] }]),
    );
    assert_eq!(apply_scheduled_restores(&w.data).unwrap(), 1);
    assert_eq!(snapshot(&w).2, 7, "Rust applies a TypeScript plan");
    assert_eq!(snapshot(&w), expected);
}
