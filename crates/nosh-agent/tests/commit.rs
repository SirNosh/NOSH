//! Task commits against TypeScript's `commitWorkspaceFiles` and `commitAllWorkspaceChanges`: each
//! scenario sets up two identical repositories (fixed identity and dates, so their starting
//! commits are the same object), runs the same steps through each implementation, and compares
//! every step's outcome and the repositories left behind. Plumbing commits carry the current time,
//! so commits are compared by tree, author, committer and message, not by ID.

use nosh_agent::commit::{CommitAuthority, commit_all_workspace_changes, commit_workspace_files};
use nosh_agent::policy::SessionOptions;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BRANCH: &str = "nosh/task";

fn run_typescript(ops: &[Value]) -> Vec<Value> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pi-adapter-ts.mjs");
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(ops).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "the TypeScript side ran");
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Git for setup and inspection, isolated from the user's configuration.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .envs([
            ("GIT_CONFIG_GLOBAL", null),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
            ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
        ])
        .output()
        .unwrap();
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A repository on `nosh/task` with `src/a.txt`, `src/b.txt`, a protected file, and an ignore rule.
fn repository(root: &Path) -> String {
    std::fs::create_dir_all(root.join("src/secret")).unwrap();
    git(root, &["init", "-q", "-b", BRANCH]).unwrap();
    for (path, body) in [
        ("src/a.txt", "a\n"),
        ("src/b.txt", "b\n"),
        ("src/secret/key.txt", "k\n"),
        (".gitignore", "*.log\n"),
        ("README.md", "readme\n"),
    ] {
        std::fs::write(root.join(path), body).unwrap();
    }
    git(root, &["add", "."]).unwrap();
    git(root, &["commit", "-q", "-m", "initial"]).unwrap();
    git(root, &["rev-parse", "HEAD"]).unwrap()
}

fn options(root: &Path, head: &str, extra: &Value) -> Value {
    let mut options = json!({
        "projectId": "prj_1", "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null,
        "taskId": "tsk_1", "agentId": "agt_0123456789abcdef0123456789abcdef", "role": "general_worker", "cwd": root.to_str().unwrap(), "packagePath": "",
        "taskPermissions": { "network": "disabled", "subprocess": "disabled", "gitCommit": true, "gitPush": false, "delegation": "request_only", "networkAllowlist": [], "allowedToolIds": ["tool_nosh.git.commit", "tool_pi.edit"] },
        "taskWorkspace": { "worktreeId": "wt_1", "branch": BRANCH, "startingCommit": head, "writeScopes": ["src/**"], "protectedScopes": ["src/secret/**"] },
    });
    merge(&mut options, extra);
    options
}

fn merge(target: &mut Value, extra: &Value) {
    for (key, value) in extra.as_object().unwrap() {
        match (target.get_mut(key), value) {
            (Some(existing @ Value::Object(_)), Value::Object(_)) => merge(existing, value),
            _ => {
                target[key] = value.clone();
            }
        }
    }
}

/// One step of a scenario, applied to a repository at `root` whose starting commit is `head`.
#[derive(Clone)]
enum Step {
    Write(&'static str, &'static str),
    Remove(&'static str),
    Mkdir(&'static str),
    Link(&'static str, &'static str),
    Git(Vec<&'static str>),
    Commit {
        extra: Value,
        heads: Vec<&'static str>,
        message: &'static str,
        paths: Vec<&'static str>,
    },
    CommitAll {
        extra: Value,
        message: &'static str,
    },
}

fn commit(paths: &[&'static str]) -> Step {
    Step::Commit {
        extra: json!({}),
        heads: vec![],
        message: "change",
        paths: paths.to_vec(),
    }
}

fn commit_all() -> Step {
    Step::CommitAll {
        extra: json!({}),
        message: "daemon commit",
    }
}

fn op(step: &Step, root: &Path, head: &str) -> Value {
    let path = |relative: &str| root.join(relative).to_str().unwrap().to_owned();
    match step {
        Step::Write(relative, body) => json!({ "writeFile": [path(relative), body] }),
        Step::Remove(relative) => json!({ "removeFile": path(relative) }),
        Step::Mkdir(relative) => json!({ "mkdir": path(relative) }),
        Step::Link(existing, relative) => json!({ "link": [path(existing), path(relative)] }),
        Step::Git(args) => json!({ "git": { "cwd": root.to_str().unwrap(), "args": args } }),
        Step::Commit {
            extra,
            heads,
            message,
            paths,
        } => {
            json!({ "commit": { "options": options(root, head, extra), "heads": heads, "message": message, "paths": paths } })
        }
        Step::CommitAll { extra, message } => {
            json!({ "commitAll": { "options": options(root, head, extra), "message": message } })
        }
    }
}

fn rust(step: &Step, root: &Path, head: &str) -> Value {
    let outcome = |result: Result<Option<nosh_agent::commit::Commit>, String>| match result {
        Ok(Some(commit)) => json!({ "ok": { "commit": commit.commit, "paths": commit.paths } }),
        Ok(None) => json!({ "ok": null }),
        Err(error) => json!({ "error": error }),
    };
    let parse = |options: Value| -> SessionOptions { serde_json::from_value(options).unwrap() };
    match step {
        Step::Write(relative, body) => {
            std::fs::write(root.join(relative), body).unwrap();
            json!({ "ok": null })
        }
        Step::Remove(relative) => {
            let target = root.join(relative);
            if target.is_dir() {
                std::fs::remove_dir_all(target).unwrap()
            } else {
                let _ = std::fs::remove_file(target);
            }
            json!({ "ok": null })
        }
        Step::Mkdir(relative) => {
            std::fs::create_dir_all(root.join(relative)).unwrap();
            json!({ "ok": null })
        }
        Step::Link(existing, relative) => {
            std::fs::hard_link(root.join(existing), root.join(relative)).unwrap();
            json!({ "ok": null })
        }
        Step::Git(args) => match git(root, args) {
            Ok(output) => json!({ "ok": output }),
            Err(error) => json!({ "error": error }),
        },
        Step::Commit {
            extra,
            heads,
            message,
            paths,
        } => {
            let mut authority = CommitAuthority {
                heads: heads
                    .iter()
                    .map(|head| (*head).to_owned())
                    .collect::<HashSet<_>>(),
            };
            let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
            outcome(
                commit_workspace_files(
                    &parse(options(root, head, extra)),
                    &mut authority,
                    message,
                    &paths,
                )
                .map(Some),
            )
        }
        Step::CommitAll { extra, message } => outcome(commit_all_workspace_changes(
            &parse(options(root, head, extra)),
            message,
        )),
    }
}

/// A step's outcome with its commit ID replaced by the commit's tree, and the root by `<ROOT>`.
fn normalize(mut outcome: Value, root: &Path) -> Value {
    if let Some(commit) = outcome
        .pointer("/ok/commit")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        outcome["ok"]["commit"] = json!(format!(
            "tree {}",
            git(root, &["rev-parse", &format!("{commit}^{{tree}}")]).unwrap()
        ));
    }
    let text = serde_json::to_string(&outcome).unwrap();
    let escaped = serde_json::to_string(root.to_str().unwrap()).unwrap();
    serde_json::from_str(&text.replace(&escaped[1..escaped.len() - 1], "<ROOT>")).unwrap()
}

/// The repository left behind: history (trees, identities, messages), index, and worktree status.
fn summary(root: &Path) -> Value {
    json!({
        "log": git(root, &["log", "--format=%T %an <%ae> %cn <%ce> %B|", BRANCH]).unwrap(),
        "head": git(root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_else(|_| "detached".into()),
        "index": git(root, &["ls-files", "--stage"]).unwrap(),
        "status": git(root, &["status", "--porcelain=v1", "-uall", "--ignored"]).unwrap(),
    })
}

#[allow(clippy::too_many_lines)]
fn scenarios() -> Vec<(&'static str, Vec<Step>)> {
    let many: Vec<&'static str> = (0..101)
        .map(|index| &*Box::leak(format!("src/f{index}.txt").into_boxed_str()))
        .collect();
    let long: &'static str = Box::leak("m".repeat(501).into_boxed_str());
    vec![
        (
            "one explicit path",
            vec![
                Step::Write("src/a.txt", "changed\n"),
                commit(&["src/a.txt"]),
            ],
        ),
        // TypeScript's isolated-index test.
        (
            "pre-staged, then dirty outside, then clean",
            vec![
                Step::Write("src/a.txt", "changed\n"),
                Step::Write("src/b.txt", "also changed\n"),
                Step::Git(vec!["add", "src/a.txt"]),
                commit(&["src/a.txt"]),
                Step::Git(vec!["reset", "-q", "--", "src/a.txt"]),
                commit(&["src/a.txt"]),
                Step::Write("src/b.txt", "b\n"),
                commit(&["src/a.txt"]),
            ],
        ),
        (
            "deletion of an indexed file",
            vec![Step::Remove("src/b.txt"), commit(&["src/b.txt"])],
        ),
        (
            "deletion of an unindexed path",
            vec![commit(&["src/none.txt"])],
        ),
        (
            "new file and an edit",
            vec![
                Step::Write("src/new.txt", "new\n"),
                Step::Write("src/a.txt", "x\n"),
                commit(&["src/new.txt", "src/a.txt"]),
            ],
        ),
        (
            "commit-all of new, edited and deleted files",
            vec![
                Step::Write("src/new.txt", "new\n"),
                Step::Write("src/a.txt", "x\n"),
                Step::Remove("src/b.txt"),
                commit_all(),
            ],
        ),
        (
            "commit-all with a change outside the scopes",
            vec![Step::Write("README.md", "changed\n"), commit_all()],
        ),
        (
            "commit-all with an untracked file outside the scopes",
            vec![Step::Write("notes.txt", "x"), commit_all()],
        ),
        (
            "commit-all with a protected change",
            vec![Step::Write("src/secret/key.txt", "leak\n"), commit_all()],
        ),
        ("commit-all of a clean worktree", vec![commit_all()]),
        (
            "commit-all ignores ignored files",
            vec![
                Step::Write("src/debug.log", "log\n"),
                commit_all(),
                Step::Write("src/a.txt", "y\n"),
                commit_all(),
            ],
        ),
        ("an unchanged explicit path", vec![commit(&["src/a.txt"])]),
        (
            "HEAD outside the lineage",
            vec![
                Step::Write("src/a.txt", "x\n"),
                Step::Commit {
                    extra: json!({ "taskWorkspace": { "startingCommit": "a".repeat(40) } }),
                    heads: vec![],
                    message: "m",
                    paths: vec!["src/a.txt"],
                },
            ],
        ),
        (
            "another branch",
            vec![
                Step::Write("src/a.txt", "x\n"),
                Step::Commit {
                    extra: json!({ "taskWorkspace": { "branch": "main" } }),
                    heads: vec![],
                    message: "m",
                    paths: vec!["src/a.txt"],
                },
            ],
        ),
        (
            "detached HEAD",
            vec![
                Step::Git(vec!["checkout", "-q", "--detach"]),
                Step::Write("src/a.txt", "x\n"),
                commit(&["src/a.txt"]),
            ],
        ),
        ("Git metadata", vec![commit(&[".git/config"])]),
        (
            "unsafe paths",
            vec![
                commit(&["src/./a.txt"]),
                commit(&["-src"]),
                commit(&["src:a"]),
                commit(&["/src/a.txt"]),
                commit(&["src//a.txt"]),
                commit(&["src/../README.md"]),
            ],
        ),
        (
            "backslash and case variants",
            vec![
                Step::Write("src/a.txt", "x\n"),
                commit(&["src\\a.txt"]),
                Step::Write("src/a.txt", "y\n"),
                commit(&["SRC/a.txt"]),
            ],
        ),
        (
            "duplicate paths",
            vec![
                Step::Write("src/a.txt", "x\n"),
                commit(&["src/a.txt", "src/a.txt"]),
            ],
        ),
        ("path counts", vec![commit(&[]), commit(&many)]),
        (
            "messages",
            vec![
                Step::Write("src/a.txt", "x\n"),
                Step::Commit {
                    extra: json!({}),
                    heads: vec![],
                    message: " \n\t ",
                    paths: vec!["src/a.txt"],
                },
                Step::Commit {
                    extra: json!({}),
                    heads: vec![],
                    message: long,
                    paths: vec!["src/a.txt"],
                },
                Step::Commit {
                    extra: json!({}),
                    heads: vec![],
                    message: "a\0b",
                    paths: vec!["src/a.txt"],
                },
                Step::Commit {
                    extra: json!({}),
                    heads: vec![],
                    message: "  trimmed  \n",
                    paths: vec!["src/a.txt"],
                },
            ],
        ),
        (
            "no commit authority",
            vec![
                Step::Write("src/a.txt", "x\n"),
                Step::Commit {
                    extra: json!({ "taskPermissions": { "gitCommit": false, "allowedToolIds": ["tool_pi.edit"] } }),
                    heads: vec![],
                    message: "m",
                    paths: vec!["src/a.txt"],
                },
            ],
        ),
        (
            "a hardlinked file",
            vec![
                Step::Write("src/a.txt", "x\n"),
                Step::Link("src/a.txt", "src/linked.txt"),
                commit(&["src/a.txt", "src/linked.txt"]),
                commit_all(),
            ],
        ),
        (
            "a tracked symlink",
            vec![
                Step::Git(vec![
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "120000,78981922613b2afb6025042ff6bd878ac1994e85,src/link",
                ]),
                Step::Git(vec!["commit", "-q", "-m", "link"]),
                Step::Write("src/a.txt", "x\n"),
                commit_all(),
            ],
        ),
        (
            "a conflicted index",
            vec![
                Step::Git(vec!["checkout", "-q", "-b", "side"]),
                Step::Write("src/a.txt", "side\n"),
                Step::Git(vec!["commit", "-q", "-am", "side"]),
                Step::Git(vec!["checkout", "-q", BRANCH]),
                Step::Write("src/a.txt", "task\n"),
                Step::Git(vec!["commit", "-q", "-am", "task"]),
                Step::Git(vec!["merge", "-q", "side"]),
                commit_all(),
            ],
        ),
        (
            "successive daemon commits",
            vec![
                Step::Write("src/a.txt", "1\n"),
                commit_all(),
                Step::Write("src/a.txt", "2\n"),
                commit_all(),
                Step::Write("src/new.txt", "3\n"),
                commit_all(),
            ],
        ),
        (
            "an unrelated HEAD",
            vec![
                Step::Git(vec!["checkout", "-q", "--orphan", "other"]),
                Step::Git(vec!["commit", "-q", "-m", "orphan"]),
                Step::Git(vec!["branch", "-q", "-f", BRANCH, "other"]),
                Step::Git(vec!["checkout", "-q", "-f", BRANCH]),
                Step::Write("src/a.txt", "x\n"),
                commit_all(),
            ],
        ),
        (
            "CRLF content is committed as is",
            vec![Step::Write("src/a.txt", "one\r\ntwo\r\n"), commit_all()],
        ),
        (
            "an indexed file replaced by a directory",
            vec![
                Step::Remove("src/b.txt"),
                Step::Mkdir("src/b.txt"),
                Step::Write("src/b.txt/inner.txt", "x"),
                commit(&["src/b.txt"]),
                commit_all(),
            ],
        ),
    ]
}

#[test]
fn commits_match_typescript() {
    let directory = tempfile::tempdir().unwrap();
    let scenarios = scenarios();
    let mut ops = Vec::new();
    let mut plans: Vec<(PathBuf, PathBuf, String, usize)> = Vec::new();
    for (index, (_, steps)) in scenarios.iter().enumerate() {
        let (ts, rs) = (
            directory.path().join(format!("ts{index}")),
            directory.path().join(format!("rs{index}")),
        );
        let head = repository(&ts);
        assert_eq!(repository(&rs), head, "identical starting commits");
        plans.push((ts.clone(), rs, head.clone(), ops.len()));
        ops.extend(steps.iter().map(|step| op(step, &ts, &head)));
    }
    let expected = run_typescript(&ops);
    let mut mismatches = Vec::new();
    let mut compared = 0;
    for ((name, steps), (ts, rs, head, offset)) in scenarios.iter().zip(&plans) {
        for (index, step) in steps.iter().enumerate() {
            let actual = normalize(rust(step, rs, head), rs);
            let mut wanted = expected[offset + index].clone();
            // The adapter marks SyntaxErrors; no commit failure is one.
            if let Some(syntax) = wanted
                .as_object_mut()
                .and_then(|object| object.remove("syntax"))
            {
                assert_eq!(syntax, json!(false), "{name}, step {index}");
            }
            let wanted = normalize(wanted, ts);
            compared += 1;
            if actual != wanted {
                mismatches.push(format!(
                    "{name}, step {index}:\n  rust: {actual}\n  ts:   {wanted}"
                ));
            }
        }
        compared += 1;
        if summary(rs) != summary(ts) {
            mismatches.push(format!(
                "{name}, repository:\n  rust: {}\n  ts:   {}",
                summary(rs),
                summary(ts)
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {compared} differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
