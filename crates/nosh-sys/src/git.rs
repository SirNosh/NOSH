//! Git plumbing and experiment worktrees, as in `packages/git-workspaces/src/index.ts`.

use crate::paths::{forward_slashes, realpath_native, resolve};
use crate::process::hidden;
use crate::{Error, Result, rejected};
use nosh_wire::{canonical_json, now_utc, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};

/// The identity NOSH authors its own merges with, so they never depend on a user Git identity.
pub const DAEMON_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "NOSH"),
    ("GIT_AUTHOR_EMAIL", "noshd@nosh.invalid"),
    ("GIT_COMMITTER_NAME", "NOSH"),
    ("GIT_COMMITTER_EMAIL", "noshd@nosh.invalid"),
];

/// Runs `git -C <cwd> <args>` and returns trimmed stdout; a failure carries trimmed stderr.
pub fn git(cwd: &Path, args: &[&str], identity: &[(&str, &str)]) -> Result<String> {
    let mut command = hidden("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .envs(worktree_git_environment(cwd))
        .envs(identity.iter().copied());
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::Git(if stderr.is_empty() {
            format!("git {} failed", args.first().unwrap_or(&""))
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether `git -C <cwd> <args>` exits successfully.
pub fn succeeds(cwd: &Path, args: &[&str]) -> bool {
    hidden("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .envs(worktree_git_environment(cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Extra environment variables, as (name, value) pairs.
pub type Environment = Vec<(String, String)>;

static TRUST: LazyLock<Mutex<HashMap<PathBuf, Environment>>> = LazyLock::new(Mutex::default);

/// Extra environment for Git inside a NOSH-created linked worktree. Filesystems without
/// ownership (FAT/exFAT, some network drives) need explicit `safe.directory` entries, and a
/// user's global list names the repository, not NOSH's per-task worktrees. The worktree is trusted
/// only when Git already trusts its owning repository under the user's own configuration; never
/// wider. Empty for anything that is not a linked worktree.
pub fn worktree_git_environment(cwd: &Path) -> Environment {
    let Ok(key) = resolve(cwd) else {
        return Vec::new();
    };
    let mut cache = TRUST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }
    let environment = linked_worktree_trust(&key).unwrap_or_default();
    cache.insert(key, environment.clone());
    environment
}

fn linked_worktree_trust(key: &Path) -> Option<Environment> {
    let dot_git = std::fs::read_to_string(key.join(".git")).ok()?;
    let git_dir = dot_git
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|dir| !dir.is_empty())?;
    // <repo>/.git/worktrees/<name> -> <repo>
    let owner = resolve(&key.join(git_dir))
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let trusted = hidden("git")
        .arg("-C")
        .arg(&owner)
        .args(["rev-parse", "--git-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !trusted {
        return None;
    }
    // `Number(process.env.GIT_CONFIG_COUNT ?? 0) || 0`
    let count: u64 = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    Some(vec![
        ("GIT_CONFIG_COUNT".into(), (count + 2).to_string()),
        (format!("GIT_CONFIG_KEY_{count}"), "safe.directory".into()),
        (format!("GIT_CONFIG_VALUE_{count}"), forward_slashes(key)),
        (
            format!("GIT_CONFIG_KEY_{}", count + 1),
            "safe.directory".into(),
        ),
        (
            format!("GIT_CONFIG_VALUE_{}", count + 1),
            forward_slashes(&owner),
        ),
    ])
}

/// An evaluated experiment's frozen provenance (`evaluated-experiments.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenExperiment {
    pub experiment_id: String,
    pub parent_experiment_id: Option<String>,
    pub branch: String,
    pub worktree: String,
    pub parent_commit: String,
    pub evaluated_commit: String,
    pub evaluation_contract_hash: String,
    pub environment_manifest_hash: String,
    pub run_command: Vec<String>,
    pub frozen_at: String,
    /// Fields a newer NOSH wrote, kept when the registry is rewritten.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub struct FreezeInput<'a> {
    pub experiment_id: &'a str,
    pub parent_experiment_id: Option<&'a str>,
    pub branch: &'a str,
    pub worktree: &'a Path,
    pub parent_commit: &'a str,
    pub evaluation_contract_hash: &'a str,
    pub environment_manifest: &'a Value,
    pub run_command: &'a [String],
}

pub struct GitWorkspaceManager {
    repository_root: PathBuf,
    worktrees_root: PathBuf,
    registry_path: PathBuf,
}

impl GitWorkspaceManager {
    pub fn new(
        repository_root: &Path,
        worktrees_root: &Path,
        state_directory: &Path,
    ) -> Result<Self> {
        let repository_root = realpath_native(&resolve(repository_root)?)?;
        let worktrees_root = resolve(worktrees_root)?;
        std::fs::create_dir_all(&worktrees_root)?;
        std::fs::create_dir_all(state_directory)?;
        let registry_path = resolve(&state_directory.join("evaluated-experiments.json"))?;
        Ok(GitWorkspaceManager {
            repository_root,
            worktrees_root,
            registry_path,
        })
    }

    /// Creates (or reuses, if it matches) the worktree `worktree_id` on `branch` from `base_commit`.
    pub fn create(&self, branch: &str, base_commit: &str, worktree_id: &str) -> Result<PathBuf> {
        git(
            &self.repository_root,
            &["check-ref-format", "--branch", branch],
            &[],
        )?;
        let path = resolve(&self.worktrees_root.join(worktree_id))?;
        // `relative(root, path).startsWith("..")`
        let escapes = path
            .strip_prefix(&self.worktrees_root)
            .map_or(true, |relative| {
                relative.to_string_lossy().starts_with("..")
            });
        if escapes {
            return rejected("Worktree path escapes configured root");
        }
        if path.exists() {
            let worktree = realpath_native(&path)?;
            if git(&worktree, &["branch", "--show-current"], &[])? != branch
                || !succeeds(
                    &worktree,
                    &["merge-base", "--is-ancestor", base_commit, "HEAD"],
                )
            {
                return rejected(format!(
                    "Existing worktree {} does not match its durable intent",
                    path.display()
                ));
            }
            return Ok(worktree);
        }
        // Check out exact blob bytes: NOSH's hardened Git plumbing ignores user config, so an autocrlf
        // checkout would make every file look modified and break exact-path commits and workspace edits.
        let path_text = path.to_string_lossy();
        let exact = ["-c", "core.autocrlf=false", "-c", "core.eol=lf"];
        if succeeds(
            &self.repository_root,
            &["show-ref", "--verify", &format!("refs/heads/{branch}")],
        ) {
            git(
                &self.repository_root,
                &[&exact[..], &["worktree", "add", &path_text, branch]].concat(),
                &[],
            )?;
        } else {
            git(
                &self.repository_root,
                &[
                    &exact[..],
                    &["worktree", "add", "-b", branch, &path_text, base_commit],
                ]
                .concat(),
                &[],
            )?;
        }
        Ok(realpath_native(&path)?)
    }

    /// Commits exactly `paths` and returns the new commit.
    pub fn commit(&self, worktree: &Path, paths: &[&str], message: &str) -> Result<String> {
        if paths.is_empty() {
            return rejected("Explicit commit paths are required");
        }
        git(worktree, &[&["add", "--"][..], paths].concat(), &[])?;
        git(worktree, &["commit", "-m", message], &[])?;
        git(worktree, &["rev-parse", "HEAD"], &[])
    }

    /// Records the evaluated commit and provenance of a clean worktree; freezing again with the
    /// same provenance returns the original record.
    pub fn freeze(&self, input: &FreezeInput<'_>) -> Result<FrozenExperiment> {
        if !git(input.worktree, &["status", "--porcelain"], &[])?.is_empty() {
            return rejected("Cannot evaluate a dirty worktree");
        }
        let evaluated_commit = git(input.worktree, &["rev-parse", "HEAD"], &[])?;
        let branch = git(input.worktree, &["branch", "--show-current"], &[])?;
        if branch != input.branch {
            return rejected(format!("Expected branch {}, found {branch}", input.branch));
        }
        let environment_manifest_hash = sha256(input.environment_manifest);
        let mut records = self.records()?;
        if let Some(prior) = records
            .iter()
            .find(|record| record.experiment_id == input.experiment_id)
        {
            let same = prior.evaluated_commit == evaluated_commit
                && prior.branch == branch
                && prior.parent_commit == input.parent_commit
                && prior.evaluation_contract_hash == input.evaluation_contract_hash
                && prior.environment_manifest_hash == environment_manifest_hash
                && canonical_json(&json!(prior.run_command))
                    == canonical_json(&json!(input.run_command));
            if !same {
                return rejected(format!(
                    "Experiment {} is already frozen with different provenance",
                    input.experiment_id
                ));
            }
            return Ok(prior.clone());
        }
        let record = FrozenExperiment {
            experiment_id: input.experiment_id.to_owned(),
            parent_experiment_id: input.parent_experiment_id.map(str::to_owned),
            branch,
            worktree: realpath_native(input.worktree)?
                .to_string_lossy()
                .into_owned(),
            parent_commit: input.parent_commit.to_owned(),
            evaluated_commit,
            evaluation_contract_hash: input.evaluation_contract_hash.to_owned(),
            environment_manifest_hash,
            run_command: input.run_command.to_vec(),
            frozen_at: now_utc(),
            extra: Map::new(),
        };
        records.push(record.clone());
        self.save(&records)?;
        Ok(record)
    }

    pub fn assert_frozen(
        &self,
        experiment_id: &str,
        commit: &str,
        evaluation_contract_hash: &str,
    ) -> Result<FrozenExperiment> {
        let Some(record) = self
            .records()?
            .into_iter()
            .find(|record| record.experiment_id == experiment_id)
        else {
            return rejected(format!("Experiment {experiment_id} is not frozen"));
        };
        if record.evaluated_commit != commit {
            return rejected("Evaluated commit is immutable");
        }
        if record.evaluation_contract_hash != evaluation_contract_hash {
            return rejected("Evaluation contract hash mismatch");
        }
        Ok(record)
    }

    /// Merges a frozen, reviewed experiment into the integration worktree (as NOSH's own identity).
    pub fn promote(
        &self,
        experiment_id: &str,
        integration_worktree: &Path,
        review_verdict: &str,
        guardrails_pass: bool,
    ) -> Result<String> {
        let Some(record) = self
            .records()?
            .into_iter()
            .find(|record| record.experiment_id == experiment_id)
        else {
            return rejected(format!("Experiment {experiment_id} is not frozen"));
        };
        if review_verdict != "PASS" || !guardrails_pass {
            return rejected("Promotion requires a passing independent review and guardrails");
        }
        if !succeeds(
            integration_worktree,
            &[
                "merge-base",
                "--is-ancestor",
                &record.evaluated_commit,
                "HEAD",
            ],
        ) {
            git(
                integration_worktree,
                &["merge", "--no-ff", "--no-edit", &record.evaluated_commit],
                DAEMON_IDENTITY,
            )?;
        }
        git(integration_worktree, &["rev-parse", "HEAD"], &[])
    }

    pub fn merge_protected(
        &self,
        source_commit: &str,
        protected_worktree: &Path,
        user_approved: bool,
    ) -> Result<String> {
        if !user_approved {
            return rejected("Protected-branch merge requires explicit user approval");
        }
        git(
            protected_worktree,
            &["merge", "--no-ff", "--no-edit", source_commit],
            &[],
        )?;
        git(protected_worktree, &["rev-parse", "HEAD"], &[])
    }

    pub fn records(&self) -> Result<Vec<FrozenExperiment>> {
        if !self.registry_path.exists() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(
            &self.registry_path,
        )?)?)
    }

    fn save(&self, records: &[FrozenExperiment]) -> Result<()> {
        write_atomic(
            &self.registry_path,
            &format!("{}\n", canonical_json(&serde_json::to_value(records)?)),
        )
    }
}

/// Writes `path` through a `.tmp` sibling and a rename, as the TypeScript modules do.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
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

    fn repository(dir: &Path) -> (PathBuf, String) {
        let repo = dir.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
        run(&repo, &["add", "README.md"]);
        run(
            &repo,
            &[
                "-c",
                "user.name=NOSH Test",
                "-c",
                "user.email=nosh@example.invalid",
                "commit",
                "-q",
                "-m",
                "baseline",
            ],
        );
        let base = run(&repo, &["rev-parse", "HEAD"]);
        (repo, base)
    }

    /// Port of "freezes evaluated commits and contract provenance" and the daemon-identity promotion.
    #[test]
    fn freezes_provenance_and_promotes_as_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, base) = repository(dir.path());
        let manager = GitWorkspaceManager::new(
            &repo,
            &dir.path().join("worktrees"),
            &dir.path().join("state"),
        )
        .unwrap();
        let worktree = manager.create("ar/test/exp1", &base, "wt1").unwrap();
        let integration = manager
            .create("ar/test/frontier", &base, "frontier")
            .unwrap();
        assert_eq!(
            manager.create("ar/test/exp1", &base, "wt1").unwrap(),
            worktree,
            "existing worktrees are reused"
        );
        assert!(
            manager
                .create("ar/test/other", &base, "wt1")
                .unwrap_err()
                .to_string()
                .contains("does not match its durable intent")
        );
        assert_eq!(
            manager
                .create("ar/test/x", &base, "..escape")
                .unwrap_err()
                .to_string(),
            "Worktree path escapes configured root"
        );

        std::fs::write(worktree.join("variant.txt"), "variant\n").unwrap();
        let identity = [
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        git(&worktree, &["add", "--", "variant.txt"], &[]).unwrap();
        git(&worktree, &["commit", "-m", "variant"], &identity).unwrap();
        let command = vec!["python".to_owned(), "train.py".to_owned()];
        let manifest = json!({ "node": "v22" });
        let input = FreezeInput {
            experiment_id: "exp_1",
            parent_experiment_id: Some("exp_base"),
            branch: "ar/test/exp1",
            worktree: &worktree,
            parent_commit: &base,
            evaluation_contract_hash: "sha256:contract",
            environment_manifest: &manifest,
            run_command: &command,
        };
        let frozen = manager.freeze(&input).unwrap();
        assert_eq!(
            manager.freeze(&input).unwrap(),
            frozen,
            "refreezing returns the original record"
        );
        assert_eq!(
            manager
                .assert_frozen("exp_1", &frozen.evaluated_commit, "sha256:contract")
                .unwrap(),
            frozen
        );
        assert_eq!(
            manager
                .assert_frozen("exp_1", &base, "sha256:contract")
                .unwrap_err()
                .to_string(),
            "Evaluated commit is immutable"
        );
        assert_eq!(
            manager
                .assert_frozen("exp_1", &frozen.evaluated_commit, "sha256:other")
                .unwrap_err()
                .to_string(),
            "Evaluation contract hash mismatch"
        );
        let other = json!({ "node": "v24" });
        assert!(
            manager
                .freeze(&FreezeInput {
                    environment_manifest: &other,
                    ..input
                })
                .unwrap_err()
                .to_string()
                .contains("already frozen with different provenance")
        );

        assert!(
            manager
                .promote("exp_1", &integration, "REVISE", true)
                .is_err()
        );
        let merged = manager
            .promote("exp_1", &integration, "PASS", true)
            .unwrap();
        assert_eq!(
            run(&integration, &["log", "-1", "--format=%an <%ae>", &merged]),
            "NOSH <noshd@nosh.invalid>"
        );
        assert_eq!(
            manager
                .merge_protected(&merged, &integration, false)
                .unwrap_err()
                .to_string(),
            "Protected-branch merge requires explicit user approval"
        );
        assert_eq!(
            manager.commit(&worktree, &[], "x").unwrap_err().to_string(),
            "Explicit commit paths are required"
        );
    }

    /// Port of "adds scoped safe.directory trust only for a linked worktree of a trusted repository".
    #[test]
    fn trusts_only_linked_worktrees_of_trusted_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, base) = repository(dir.path());
        assert!(worktree_git_environment(&repo).is_empty());
        let manager = GitWorkspaceManager::new(
            &repo,
            &repo.join(".nosh/worktrees"),
            &repo.join(".nosh/git"),
        )
        .unwrap();
        let worktree = manager
            .create("nosh/task-trust", &base, "wt_trust")
            .unwrap();
        let values: Vec<String> = worktree_git_environment(&worktree)
            .into_iter()
            .filter(|(k, _)| k.starts_with("GIT_CONFIG_VALUE_"))
            .map(|(_, v)| v.to_lowercase())
            .collect();
        assert!(
            values.contains(&forward_slashes(&worktree).to_lowercase()),
            "{values:?}"
        );
        assert!(
            values.contains(&forward_slashes(&realpath_native(&repo).unwrap()).to_lowercase()),
            "{values:?}"
        );
    }

    /// Port of "matches committed blobs even when the user's Git converts line endings".
    #[test]
    fn worktrees_check_out_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "core.autocrlf", "true"]);
        std::fs::write(repo.join("config.json"), "{\n  \"threshold\": 0.5\n}\n").unwrap();
        run(&repo, &["add", "config.json"]);
        run(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "a",
            ],
        );
        let manager = GitWorkspaceManager::new(
            &repo,
            &repo.join(".nosh/worktrees"),
            &repo.join(".nosh/git"),
        )
        .unwrap();
        let worktree = manager
            .create(
                "nosh/task-eol",
                &run(&repo, &["rev-parse", "HEAD"]),
                "wt_eol",
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(worktree.join("config.json")).unwrap(),
            "{\n  \"threshold\": 0.5\n}\n"
        );
        assert_eq!(
            run(
                &worktree,
                &["-c", "core.autocrlf=false", "status", "--porcelain"]
            ),
            ""
        );
    }
}
