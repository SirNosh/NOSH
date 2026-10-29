//! Port of `apps/noshd/src/task-postflight.test.ts`.

use nosh_orchestration::task_postflight::git_changed_paths;
use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn includes_both_sides_of_renames_and_deleted_protected_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init"]);
    git(root, &["config", "user.email", "fixture@nosh.dev"]);
    git(root, &["config", "user.name", "NOSH Fixture"]);
    std::fs::create_dir_all(root.join(".nosh/contracts")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".nosh/contracts/retired.json"), "{}\n").unwrap();
    std::fs::write(
        root.join("src/old-name.ts"),
        "export const oldName = true;\n",
    )
    .unwrap();
    git(root, &["add", "-f", ".nosh/contracts/retired.json"]);
    git(root, &["add", "src/old-name.ts"]);
    git(root, &["commit", "-m", "baseline"]);
    let starting = git(root, &["rev-parse", "HEAD"]);

    std::fs::create_dir(root.join("lib")).unwrap();
    std::fs::rename(root.join("src/old-name.ts"), root.join("lib/new-name.ts")).unwrap();
    std::fs::remove_file(root.join(".nosh/contracts/retired.json")).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "rename and delete"]);
    let ending = git(root, &["rev-parse", "HEAD"]);

    let changed = git_changed_paths(root, &starting, &ending).unwrap();
    for path in [
        "src/old-name.ts",
        "lib/new-name.ts",
        ".nosh/contracts/retired.json",
    ] {
        assert!(
            changed.iter().any(|changed| changed == path),
            "{path} missing from {changed:?}"
        );
    }
}
