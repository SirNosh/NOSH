//! Frozen-experiment provenance written by one implementation must be accepted by the other
//! (`evaluated-experiments.json`), including refreezing, which compares hashes.

use nosh_sys::git::{FreezeInput, GitWorkspaceManager, git};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn typescript(construct: Value, calls: Value) -> Vec<Value> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let request = json!({ "module": "packages/git-workspaces/dist/index.js", "construct": construct, "calls": calls });
    let mut child = Command::new("node")
        .arg(root.join("scripts/ts-call.mjs"))
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
    serde_json::from_slice(&output.stdout).unwrap()
}

fn setup(dir: &Path) -> (PathBuf, String, GitWorkspaceManager, Value) {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    run(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
    run(&["add", "README.md"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "baseline",
    ]);
    let base = git(&repo, &["rev-parse", "HEAD"], &[]).unwrap();
    let (worktrees, state) = (dir.join("worktrees"), dir.join("state"));
    let manager = GitWorkspaceManager::new(&repo, &worktrees, &state).unwrap();
    let construct = json!({ "name": "GitWorkspaceManager", "args": [repo, worktrees, state] });
    (repo, base, manager, construct)
}

#[test]
fn frozen_provenance_crosses_implementations() {
    let dir = tempfile::tempdir().unwrap();
    let (_repo, base, manager, construct) = setup(dir.path());
    let worktree = manager.create("ar/x/exp1", &base, "wt1").unwrap();
    let identity = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
    ];
    std::fs::write(worktree.join("v.txt"), "v\n").unwrap();
    git(&worktree, &["add", "--", "v.txt"], &[]).unwrap();
    git(&worktree, &["commit", "-m", "v"], &identity).unwrap();
    let manifest = json!({ "python": "3.12", "weights": [0.5, 1e21] });
    let command = vec!["python".to_owned(), "train.py".to_owned()];
    let input = FreezeInput {
        experiment_id: "exp_rust",
        parent_experiment_id: None,
        branch: "ar/x/exp1",
        worktree: &worktree,
        parent_commit: &base,
        evaluation_contract_hash: "sha256:c",
        environment_manifest: &manifest,
        run_command: &command,
    };
    let ours = manager.freeze(&input).unwrap();

    // TypeScript accepts the Rust record, refreezes it to the same record, then freezes its own.
    let ts_input = |id: &str| json!({ "experimentId": id, "parentExperimentId": null, "branch": "ar/x/exp1", "worktree": worktree, "parentCommit": base, "evaluationContractHash": "sha256:c", "environmentManifest": manifest, "runCommand": command });
    let theirs = typescript(
        construct,
        json!([
            { "method": "assertFrozen", "args": ["exp_rust", ours.evaluated_commit, "sha256:c"] },
            { "method": "freeze", "args": [ts_input("exp_rust")] },
            { "method": "freeze", "args": [ts_input("exp_ts")] },
        ]),
    );
    let ours_json = serde_json::to_value(&ours).unwrap();
    assert_eq!(
        theirs[0]["ok"], ours_json,
        "TypeScript reads the Rust record unchanged"
    );
    assert_eq!(
        theirs[1]["ok"], ours_json,
        "refreezing in TypeScript matches the Rust hashes"
    );

    // Rust accepts the TypeScript record and refreezes it to the same record.
    let ts_record = &theirs[2]["ok"];
    let rust_view = manager
        .assert_frozen(
            "exp_ts",
            ts_record["evaluatedCommit"].as_str().unwrap(),
            "sha256:c",
        )
        .unwrap();
    assert_eq!(serde_json::to_value(&rust_view).unwrap(), *ts_record);
    let refrozen = manager
        .freeze(&FreezeInput {
            experiment_id: "exp_ts",
            ..input
        })
        .unwrap();
    assert_eq!(serde_json::to_value(&refrozen).unwrap(), *ts_record);
    assert_eq!(manager.records().unwrap().len(), 2);
}
