//! Project backups, as in `packages/persistence/src/backup.ts`.
//!
//! A backup is a directory `<data>/backups/<projectId>-<ISO time>` holding the project database,
//! the Project's research units, its Jobs, and a Git bundle with the ref and HEAD state, all
//! inventoried by SHA-256 in `manifest.json` (`nosh-backup-v1`). Every path is checked for
//! symbolic links and junctions, and Git runs with a hardened environment that ignores the
//! user's system and global configuration except for a few safe checkout settings.

use crate::{Error, Result, rejected};
use nosh_sys::paths::{relative_inside, resolve};
use nosh_sys::process::{hidden, run_with_input};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// A registered Project (`RegisteredProject`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredProject {
    pub project_id: String,
    pub repository_root: String,
    pub database_path: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProject {
    pub project_id: String,
    pub repository_root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBundleInfo {
    pub has_refs: bool,
    pub head_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// `manifest.json`; fields in `JSON.stringify` order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub format: String,
    pub created_at: String,
    pub project: BackupProject,
    pub sqlite_schema_version: i64,
    pub includes_large_artifacts: bool,
    pub git_bundle: GitBundleInfo,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub backup_id: String,
    pub path: String,
    pub created_at: String,
    pub project: BackupProject,
    pub sqlite_schema_version: i64,
    pub includes_large_artifacts: bool,
}

/// `git-head.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHead {
    pub version: u8,
    pub state: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub commit: Option<String>,
}

pub(crate) const BACKUP_UNITS: [&str; 9] = [
    ".nosh/contracts",
    ".nosh/events",
    ".nosh/sessions",
    ".nosh/project.json",
    ".nosh/schema-lock.json",
    ".nosh/artifacts",
    "docs/paper.md",
    "docs/paper.bib",
    "docs/figures",
];
pub(crate) const DATABASE_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

pub fn create_project_backup(
    data_directory: &Path,
    project: &RegisteredProject,
) -> Result<BackupSummary> {
    let data_root = resolve(data_directory)?;
    let backups_root = data_root.join("backups");
    let repository = Path::new(&project.repository_root);
    let database = Path::new(&project.database_path);
    assert_no_linked_ancestors(repository, repository, "Project repository")?;
    assert_git_clean(repository, &[])?;
    let git_head = capture_git_head(repository)?;
    if let Some(commit) = &git_head.commit {
        git_tree_entries(repository, commit)?;
    }
    assert_no_linked_ancestors(&data_root, &backups_root, "Backup root")?;
    assert_no_linked_ancestors(parent(database), database, "Project database")?;
    let unlinked_file = std::fs::symlink_metadata(database)
        .is_ok_and(|stat| stat.is_file() && !stat.is_symlink())
        && nosh_sys::fs::hard_link_count(database).is_ok_and(|links| links <= 1);
    if !unlinked_file {
        return rejected("Project database must already exist as an unlinked regular file");
    }
    std::fs::create_dir_all(&backups_root)?;
    assert_no_linked_ancestors(&data_root, &backups_root, "Backup root")?;
    let backup_id = format!(
        "{}-{}",
        project.project_id,
        nosh_wire::now_utc().replace(':', "-")
    );
    let destination = backups_root.join(&backup_id);
    let staging = with_suffix(&destination, ".creating");
    assert_no_linked_ancestors(&data_root, &staging, "Backup staging")?;
    if destination.exists() || staging.exists() {
        return rejected(format!("Backup destination already exists: {backup_id}"));
    }
    std::fs::create_dir_all(&staging)?;
    let created = (|| -> Result<BackupSummary> {
        let sqlite_schema_version = backup_database(database, &staging.join("nosh.sqlite"))?;
        for unit in BACKUP_UNITS {
            copy_if_present(repository, &staging, unit)?;
        }
        copy_project_jobs(&data_root, &staging, &project.project_id)?;
        let initial_refs = run_git(
            repository,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        );
        if !initial_refs.ok {
            return rejected("Backup Git ref snapshot failed");
        }
        let initial_ref_map = refs_from(&initial_refs.stdout)?;
        assert_git_head_consistency(&git_head, &initial_ref_map, "Project Git HEAD")?;
        let has_refs = !initial_ref_map.is_empty();
        let has_bundle = has_refs || git_head.commit.is_some();
        let (mut bundle_refs, mut bundle_commits) = (String::new(), String::new());
        if has_bundle {
            let bundle = staging.join("git.bundle");
            let bundle_text = bundle.to_string_lossy().into_owned();
            let mut args = vec!["bundle", "create", bundle_text.as_str()];
            if has_refs {
                args.push("--all");
            }
            if git_head.commit.is_some() {
                args.push("HEAD");
            }
            if !run_git(repository, &args).ok {
                return rejected("Backup Git bundle creation failed");
            }
            let (refs, commits) = snapshot_bundle(&bundle)?;
            bundle_refs = refs;
            let mut seen = HashSet::new();
            let listed = commits
                .lines()
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .chain(git_head.commit.clone());
            bundle_commits = listed
                .filter(|commit| seen.insert(commit.clone()))
                .collect::<Vec<_>>()
                .join("\n");
            if has_refs && bundle_refs.trim().is_empty() {
                return rejected("Backup Git bundle contains no restorable refs");
            }
        }
        std::fs::write(staging.join("git-refs.txt"), &bundle_refs)?;
        std::fs::write(staging.join("required-commits.txt"), &bundle_commits)?;
        write_json_atomic(
            &staging.join("git-bundle.json"),
            &json!({ "version": 1, "hasRefs": has_refs, "headCommit": git_head.commit }),
        )?;
        write_json_atomic(
            &staging.join("git-head.json"),
            &serde_json::to_value(&git_head)?,
        )?;
        let manifest = BackupManifest {
            format: "nosh-backup-v1".into(),
            created_at: nosh_wire::now_utc(),
            project: BackupProject {
                project_id: project.project_id.clone(),
                repository_root: project.repository_root.clone(),
            },
            sqlite_schema_version,
            includes_large_artifacts: staging.join(".nosh").join("artifacts").exists(),
            git_bundle: GitBundleInfo {
                has_refs,
                head_commit: git_head.commit.clone(),
            },
            files: hash_files(&staging)?,
        };
        write_json_atomic(
            &staging.join("manifest.json"),
            &serde_json::to_value(&manifest)?,
        )?;
        verify_backup(&staging)?;
        std::fs::rename(&staging, &destination)?;
        Ok(BackupSummary {
            backup_id: backup_id.clone(),
            path: destination.to_string_lossy().into_owned(),
            created_at: manifest.created_at,
            project: manifest.project,
            sqlite_schema_version,
            includes_large_artifacts: manifest.includes_large_artifacts,
        })
    })();
    if created.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    created
}

/// The Project's valid backups, newest first; unreadable or tampered backups are skipped.
pub fn list_project_backups(data_directory: &Path, project_id: &str) -> Result<Vec<BackupSummary>> {
    let data_root = resolve(data_directory)?;
    let root = data_root.join("backups");
    assert_no_linked_ancestors(&data_root, &root, "Backup root")?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = root.join(entry.file_name());
        let Ok(manifest) = verify_backup(&path) else {
            continue;
        };
        if manifest.project.project_id == project_id {
            out.push(BackupSummary {
                backup_id: entry.file_name().to_string_lossy().into_owned(),
                path: path.to_string_lossy().into_owned(),
                created_at: manifest.created_at,
                project: manifest.project,
                sqlite_schema_version: manifest.sqlite_schema_version,
                includes_large_artifacts: manifest.includes_large_artifacts,
            });
        }
    }
    out.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(out)
}

/// The directory of an existing backup, refusing IDs and paths that could escape the backup root.
pub fn backup_path(data_directory: &Path, backup_id: &str) -> Result<PathBuf> {
    let valid = backup_id != "."
        && backup_id != ".."
        && !backup_id.is_empty()
        && backup_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !valid {
        return rejected("Backup ID is invalid");
    }
    let data_root = resolve(data_directory)?;
    let root = data_root.join("backups");
    assert_no_linked_ancestors(&data_root, &root, "Backup root")?;
    let path = resolve(&root.join(backup_id))?;
    assert_no_linked_ancestors(&data_root, &path, "Backup path")?;
    let is_directory =
        std::fs::symlink_metadata(&path).is_ok_and(|stat| !stat.is_symlink()) && path.is_dir();
    if !contained(&root, &path) || !is_directory {
        return rejected("Backup does not exist");
    }
    Ok(path)
}

/// Checks a backup completely: inventory and hashes, Artifact objects, Git metadata, and the
/// Git bundle (verified in a scratch repository).
pub fn verify_backup(backup_directory: &Path) -> Result<BackupManifest> {
    let root = resolve(backup_directory)?;
    assert_no_linked_ancestors(&root, &root, "Backup root")?;
    if std::fs::symlink_metadata(&root).map_or(true, |stat| stat.is_symlink()) || !root.is_dir() {
        return rejected("Backup root is missing, symlinked, or not a directory");
    }
    assert_regular_contained_file(&root, "manifest.json", "Backup manifest")?;
    let manifest = read_backup_manifest(&root)?;
    let mut declared = manifest.files.clone();
    declared.sort_by(|a, b| a.path.cmp(&b.path));
    let mut paths = HashSet::new();
    for entry in &declared {
        let hex = entry.sha256.len() == 64
            && entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if entry.path.is_empty()
            || entry.path == "manifest.json"
            || !hex
            || !paths.insert(entry.path.clone())
        {
            return rejected(format!("Malformed backup file entry: {}", entry.path));
        }
        let path = resolve(&root.join(&entry.path))?;
        let regular =
            std::fs::symlink_metadata(&path).is_ok_and(|stat| stat.is_file() && !stat.is_symlink());
        if !contained(&root, &path) || !regular {
            return rejected(format!(
                "Backup file is missing, symlinked, or escapes backup root: {}",
                entry.path
            ));
        }
    }
    let actual: Vec<FileEntry> = hash_files(&root)?
        .into_iter()
        .filter(|entry| entry.path != "manifest.json")
        .collect();
    if actual.len() != declared.len() {
        return rejected("Backup inventory does not match manifest");
    }
    for (expected, observed) in declared.iter().zip(&actual) {
        if expected != observed {
            return rejected(format!("Backup integrity check failed: {}", expected.path));
        }
    }
    verify_artifact_objects(&root)?;
    for name in [
        "git-bundle.json",
        "git-head.json",
        "git-refs.txt",
        "required-commits.txt",
    ] {
        assert_regular_contained_file(&root, name, "Backup Git metadata")?;
    }
    let bundle_metadata: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("git-bundle.json"))?)?;
    let git_head = read_git_head(&root)?;
    let head_commit_ok = match &bundle_metadata["headCommit"] {
        Value::Null => true,
        Value::String(commit) => safe_git_object_id(commit),
        _ => false,
    };
    let consistent = bundle_metadata["version"] == 1
        && bundle_metadata["hasRefs"].is_boolean()
        && head_commit_ok
        && bundle_metadata["hasRefs"] == manifest.git_bundle.has_refs
        && bundle_metadata["headCommit"] == json!(manifest.git_bundle.head_commit)
        && git_head.commit == manifest.git_bundle.head_commit;
    if !consistent {
        return rejected("Backup Git bundle metadata is inconsistent with the manifest");
    }
    let refs = refs_from(&std::fs::read_to_string(root.join("git-refs.txt"))?)?;
    if manifest.git_bundle.has_refs != !refs.is_empty() {
        return rejected("Backup Git bundle ref metadata is inconsistent with the ref inventory");
    }
    assert_git_head_consistency(&git_head, &refs, "Backup Git HEAD")?;
    let commits = read_required_commits(&root)?;
    if git_head
        .commit
        .as_ref()
        .is_some_and(|commit| !commits.contains(commit))
    {
        return rejected("Backup HEAD commit is absent from required commits");
    }
    let bundle = root.join("git.bundle");
    if manifest.git_bundle.has_refs || git_head.commit.is_some() {
        verify_bundle(&bundle, &commits)?;
    } else {
        if !commits.is_empty() {
            return rejected("Unborn backup must not require Git commits");
        }
        if bundle.exists() {
            return rejected("Unborn backup must not contain a Git bundle");
        }
    }
    Ok(manifest)
}

/// Copies the Project's Jobs (records naming `project_id`) into `destination/jobs`.
pub fn copy_project_jobs(
    data_directory: &Path,
    destination: &Path,
    project_id: &str,
) -> Result<()> {
    let jobs_root = resolve(data_directory)?.join("jobs");
    if !jobs_root.exists() {
        return Ok(());
    }
    if std::fs::symlink_metadata(&jobs_root)?.is_symlink() {
        return rejected("Cannot back up jobs through a symlink");
    }
    let matching = jobs_in(&jobs_root, project_id, "Cannot back up", &HashSet::new())?;
    if matching.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(destination.join("jobs"))?;
    for job_id in matching {
        let source = jobs_root.join(&job_id);
        reject_symlinks(&source)?;
        copy_tree(&source, &destination.join("jobs").join(&job_id))?;
    }
    Ok(())
}

/// Every file under `root` with its `/`-separated relative path, SHA-256, and size, sorted by path.
pub fn hash_files(root: &Path) -> Result<Vec<FileEntry>> {
    fn visit(root: &Path, directory: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
        if std::fs::symlink_metadata(directory)?.is_symlink() {
            return rejected(format!(
                "Symlinked backup path is forbidden: {}",
                directory.display()
            ));
        }
        for entry in std::fs::read_dir(directory)? {
            let path = directory.join(entry?.file_name());
            let stat = std::fs::symlink_metadata(&path)?;
            if stat.is_symlink() {
                return rejected(format!(
                    "Symlinked backup path is forbidden: {}",
                    path.display()
                ));
            } else if stat.is_dir() {
                visit(root, &path, out)?;
            } else if stat.is_file() {
                let bytes = std::fs::read(&path)?;
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(FileEntry {
                    path: relative,
                    sha256: hex_sha256(&bytes),
                    bytes: bytes.len() as u64,
                });
            } else {
                return rejected(format!("Unsupported backup file type: {}", path.display()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, root, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Online SQLite backup of the (migrated) project database; returns its schema version.
fn backup_database(source: &Path, destination: &Path) -> Result<i64> {
    let database = crate::open_database(source)?;
    {
        let mut target = rusqlite::Connection::open(destination)?;
        rusqlite::backup::Backup::new(&database, &mut target)?.run_to_completion(
            256,
            Duration::ZERO,
            None,
        )?;
    }
    for suffix in DATABASE_SIDECARS {
        if with_suffix(destination, suffix).exists() {
            return rejected(format!("Backup database left a {suffix} sidecar"));
        }
    }
    Ok(database.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?)
}

fn read_backup_manifest(root: &Path) -> Result<BackupManifest> {
    let path = root.join("manifest.json");
    if !path.exists() {
        return rejected("Backup manifest.json is missing");
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let non_empty = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
    let integer = |v: &Value| v.as_i64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0);
    let head_ok = match &value["gitBundle"]["headCommit"] {
        Value::Null => true,
        Value::String(commit) => safe_git_object_id(commit),
        _ => false,
    };
    let well_formed = value["format"] == "nosh-backup-v1"
        && non_empty(&value["createdAt"])
        && non_empty(&value["project"]["projectId"])
        && non_empty(&value["project"]["repositoryRoot"])
        && value["files"].is_array()
        && integer(&value["sqliteSchemaVersion"])
        && value["includesLargeArtifacts"].is_boolean()
        && value["gitBundle"]["hasRefs"].is_boolean()
        && head_ok;
    if !well_formed {
        return rejected("Unsupported or malformed backup manifest");
    }
    let malformed_entry = |entry: &Value| {
        Error::Rejected(format!(
            "Malformed backup file entry: {}",
            entry["path"].as_str().unwrap_or("undefined")
        ))
    };
    for entry in value["files"].as_array().into_iter().flatten() {
        let bytes_ok = entry["bytes"].as_u64().is_some()
            || entry["bytes"]
                .as_f64()
                .is_some_and(|f| f.fract() == 0.0 && f >= 0.0);
        if !entry["path"].is_string() || !entry["sha256"].is_string() || !bytes_ok {
            return Err(malformed_entry(entry));
        }
    }
    let mut normalized = value;
    nosh_wire::normalize_numbers(&mut normalized);
    Ok(serde_json::from_value(normalized)?)
}

/// Job directories under `root` whose `job.json` names `project_id` (records are read only for
/// their `projectId`).
pub(crate) fn jobs_in(
    root: &Path,
    project_id: &str,
    label: &str,
    excluded: &HashSet<PathBuf>,
) -> Result<Vec<String>> {
    Ok(job_records(root, label, excluded)?
        .into_iter()
        .filter(|(_, owner)| owner == project_id)
        .map(|(name, _)| name)
        .collect())
}

/// `(job directory name, projectId)` for every Job under `root`.
fn job_records(
    root: &Path,
    label: &str,
    excluded: &HashSet<PathBuf>,
) -> Result<Vec<(String, String)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if std::fs::symlink_metadata(root)?.is_symlink() {
        return rejected(format!("{label} jobs root is symlinked"));
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if excluded.contains(&resolve(&root.join(&name))?) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return rejected(format!("{label} Job {name}: symlinks are forbidden"));
        }
        if !kind.is_dir() {
            continue;
        }
        let record_path = root.join(&name).join("job.json");
        if std::fs::symlink_metadata(&record_path).map_or(true, |stat| stat.is_symlink()) {
            return rejected(format!("{label} Job {name}: missing or symlinked job.json"));
        }
        let malformed = || Error::Rejected(format!("{label} Job {name}: malformed job.json"));
        let record: Value = std::fs::read_to_string(&record_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .ok_or_else(malformed)?;
        let owner = record
            .get("projectId")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        out.push((name, owner.to_owned()));
    }
    Ok(out)
}

/// For other Projects' Jobs, a fingerprint of their files (`JSON.stringify(hashFiles(path))`).
pub(crate) fn other_job_fingerprints(
    root: &Path,
    project_id: &str,
    label: &str,
    excluded: &HashSet<PathBuf>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (name, owner) in job_records(root, label, excluded)? {
        if owner != project_id {
            out.insert(
                name.clone(),
                serde_json::to_string(&hash_files(&root.join(&name))?)?,
            );
        }
    }
    Ok(out)
}

fn copy_if_present(source_root: &Path, destination_root: &Path, unit: &str) -> Result<()> {
    let source = source_root.join(unit);
    assert_no_linked_ancestors(source_root, &source, "Backup source")?;
    let Ok(stat) = std::fs::symlink_metadata(&source) else {
        return Ok(());
    };
    if stat.is_symlink() {
        return rejected(format!(
            "Symlinked backup source is forbidden: {}",
            source.display()
        ));
    }
    let target = destination_root.join(unit);
    std::fs::create_dir_all(parent(&target))?;
    reject_symlinks(&source)?;
    copy_tree(&source, &target)
}

/// `cpSync(source, target, { recursive: true })`: files overwrite, directories merge.
pub(crate) fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let name = entry?.file_name();
            copy_tree(&source.join(&name), &target.join(&name))?;
        }
    } else {
        std::fs::copy(source, target)?;
    }
    Ok(())
}

fn verify_artifact_objects(root: &Path) -> Result<()> {
    let artifact_root = root.join(".nosh").join("artifacts");
    let manifest_path = artifact_root.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    assert_no_linked_ancestors(root, &manifest_path, "Artifact manifest")?;
    let records: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let Value::Array(records) = records else {
        return rejected("Artifact manifest is malformed");
    };
    let mut identities = HashSet::new();
    for record in &records {
        let malformed = || Error::Rejected("Artifact manifest record is malformed".into());
        let Value::Object(artifact) = record else {
            return Err(malformed());
        };
        let artifact_id = artifact
            .get("artifactId")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let version = artifact
            .get("version")
            .and_then(Value::as_f64)
            .filter(|v| v.fract() == 0.0 && *v >= 1.0)
            .ok_or_else(malformed)?;
        let stored_path = artifact
            .get("storedPath")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let content_hash = artifact
            .get("contentHash")
            .and_then(Value::as_str)
            .filter(|h| {
                h.strip_prefix("sha256:").is_some_and(|hex| {
                    hex.len() == 64
                        && hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            })
            .ok_or_else(malformed)?;
        let size = artifact
            .get("sizeBytes")
            .and_then(Value::as_f64)
            .filter(|s| s.fract() == 0.0 && *s >= 0.0)
            .ok_or_else(malformed)?;
        let identity = format!("{artifact_id}:{version}");
        if !identities.insert(identity.clone()) {
            return rejected(format!("Ambiguous artifact version: {identity}"));
        }
        let stored = Path::new(stored_path);
        let path = if stored.is_absolute() {
            resolve(stored)?
        } else {
            resolve(&artifact_root.join(stored))?
        };
        if !contained(&artifact_root, &path) {
            return rejected(format!(
                "Artifact path escapes artifact root: {artifact_id}"
            ));
        }
        assert_no_linked_ancestors(root, &path, &format!("Artifact {artifact_id}"))?;
        if !std::fs::symlink_metadata(&path).is_ok_and(|stat| stat.is_file() && !stat.is_symlink())
        {
            return rejected(format!(
                "Artifact object is missing or linked: {artifact_id}"
            ));
        }
        let bytes = std::fs::read(&path)?;
        if bytes.len() as f64 != size || format!("sha256:{}", hex_sha256(&bytes)) != content_hash {
            return rejected(format!("Artifact object integrity failed: {artifact_id}"));
        }
    }
    Ok(())
}

// ---- Path safety (Node's win32 semantics) ----

pub(crate) fn contained(root: &Path, candidate: &Path) -> bool {
    relative_inside(root, candidate).is_ok_and(|inside| inside.is_some())
}

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        resolve(path).map(|p| {
            if cfg!(windows) {
                p.to_string_lossy().to_lowercase()
            } else {
                p.to_string_lossy().into_owned()
            }
        })
    };
    matches!((normalize(left), normalize(right)), (Ok(a), Ok(b)) if a == b)
}

/// `candidate` must be inside `allowed_root`, and nothing from the filesystem root down to it may
/// be a symbolic link or junction.
pub(crate) fn assert_no_linked_ancestors(
    allowed_root: &Path,
    candidate: &Path,
    label: &str,
) -> Result<()> {
    let root = resolve(allowed_root)?;
    let target = resolve(candidate)?;
    let Some(inside) = relative_inside(&root, &target)? else {
        return rejected(format!("{label} escapes its allowed root"));
    };
    assert_unlinked_from_filesystem_root(&root, label)?;
    let mut path = root;
    for segment in inside.components() {
        path.push(segment);
        if !inspect_unlinked(&path, label)? {
            break;
        }
    }
    Ok(())
}

pub(crate) fn assert_unlinked_from_filesystem_root(target: &Path, label: &str) -> Result<()> {
    let target = resolve(target)?;
    let mut path = PathBuf::new();
    for component in target.components() {
        path.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        if !inspect_unlinked(&path, label)? {
            break;
        }
    }
    Ok(())
}

fn inspect_unlinked(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(false),
        Ok(stat) if stat.is_symlink() => rejected(format!(
            "{label} traverses a symlink or reparse point: {}",
            path.display()
        )),
        Ok(_) => Ok(true),
    }
}

pub(crate) fn assert_regular_contained_file(
    root: &Path,
    name: &str,
    label: &str,
) -> Result<PathBuf> {
    let path = resolve(&root.join(name))?;
    let regular =
        std::fs::symlink_metadata(&path).is_ok_and(|stat| stat.is_file() && !stat.is_symlink());
    if !contained(root, &path) || !regular {
        return rejected(format!(
            "{label} is missing, linked, or escapes backup root: {name}"
        ));
    }
    Ok(path)
}

pub(crate) fn reject_symlinks(root: &Path) -> Result<()> {
    if std::fs::symlink_metadata(root)?.is_symlink() {
        return rejected(format!(
            "Symlinked backup source is forbidden: {}",
            root.display()
        ));
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        reject_symlinks(&root.join(entry?.file_name()))?;
    }
    Ok(())
}

pub(crate) fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    std::fs::create_dir_all(parent(path))?;
    let temporary = with_suffix(path, ".tmp");
    std::fs::write(&temporary, format!("{value}\n"))?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

pub(crate) fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(suffix);
    PathBuf::from(text)
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

// ---- Hardened Git ----

pub(crate) struct GitOutput {
    pub ok: bool,
    /// The exit status, or `None` when the command timed out or could not start.
    pub status: Option<i32>,
    pub stdout: String,
    pub bytes: Vec<u8>,
}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The inherited environment without any `GIT_*` variable (keeping PATH and the rest).
fn without_git_variables() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| !key.to_string_lossy().to_uppercase().starts_with("GIT_"))
        .collect()
}

/// `gitEnvironmentWith`: hooks, external diff, pagers, prompts, fsmonitor, signing, renames, and
/// attribute files disabled; system and global configuration ignored.
fn hardened_environment(safe_checkout: &[(String, String)]) -> Vec<(OsString, OsString)> {
    let fixed = [
        ("core.hooksPath", NULL_DEVICE),
        ("diff.external", ""),
        ("core.pager", "cat"),
        ("core.fsmonitor", "false"),
        ("commit.gpgSign", "false"),
        ("diff.renames", "false"),
        ("core.attributesFile", NULL_DEVICE),
    ];
    let mut environment = without_git_variables();
    for (key, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", NULL_DEVICE),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("GIT_EDITOR", "true"),
        ("GIT_ASKPASS", "true"),
    ] {
        environment.push((key.into(), value.into()));
    }
    let pairs: Vec<(String, String)> = fixed
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .chain(safe_checkout.iter().cloned())
        .collect();
    environment.push(("GIT_CONFIG_COUNT".into(), pairs.len().to_string().into()));
    for (index, (key, value)) in pairs.into_iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{index}").into(), key.into()));
        environment.push((format!("GIT_CONFIG_VALUE_{index}").into(), value.into()));
    }
    environment
}

/// The user's global `core.autocrlf|filemode|ignorecase|symlinks`, so `git status` sees the
/// checkout the way the user's own Git does.
fn safe_global_checkout_config(repository: &Path) -> Result<Vec<(String, String)>> {
    let mut environment = without_git_variables();
    environment.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    environment.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    let result = git_with(
        repository,
        &[
            "config",
            "--global",
            "--get-regexp",
            r"^core\.(autocrlf|filemode|ignorecase|symlinks)$",
        ],
        environment,
        None,
    );
    match result.status {
        Some(1) => return Ok(Vec::new()),
        Some(0) => {}
        _ => return rejected("Could not read safe global Git checkout configuration"),
    }
    let mut values: Vec<(String, String)> = Vec::new();
    for line in result
        .stdout
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
    {
        let (key, value) = line
            .split_once(char::is_whitespace)
            .filter(|(key, _)| {
                matches!(
                    *key,
                    "core.autocrlf" | "core.filemode" | "core.ignorecase" | "core.symlinks"
                )
            })
            .ok_or_else(|| {
                Error::Rejected("Global Git checkout configuration is malformed".into())
            })?;
        match values.iter_mut().find(|(existing, _)| existing == key) {
            Some(entry) => entry.1 = value.to_owned(),
            None => values.push((key.to_owned(), value.to_owned())),
        }
    }
    Ok(values)
}

fn git_with(
    repository: &Path,
    args: &[&str],
    environment: Vec<(OsString, OsString)>,
    input: Option<&str>,
) -> GitOutput {
    let mut command = hidden("git");
    command
        .arg("-C")
        .arg(repository)
        .args(args)
        .env_clear()
        .envs(environment);
    git_output(&mut command, input)
}

fn git_output(command: &mut std::process::Command, input: Option<&str>) -> GitOutput {
    // TypeScript also caps output (16 MB, 64 MB for blobs); Rust reads it all.
    match run_with_input(
        command,
        input.map(|text| text.as_bytes().to_vec()),
        GIT_TIMEOUT,
    ) {
        Ok((status, output)) => {
            let code = status.and_then(|s| s.code());
            GitOutput {
                ok: code == Some(0),
                status: code,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                bytes: output.stdout,
            }
        }
        Err(_) => GitOutput {
            ok: false,
            status: None,
            stdout: String::new(),
            bytes: Vec::new(),
        },
    }
}

pub(crate) fn run_git(repository: &Path, args: &[&str]) -> GitOutput {
    git_with(repository, args, hardened_environment(&[]), None)
}

pub(crate) fn run_git_input(repository: &Path, args: &[&str], input: &str) -> GitOutput {
    git_with(repository, args, hardened_environment(&[]), Some(input))
}

fn run_git_status(repository: &Path, args: &[&str]) -> Result<GitOutput> {
    Ok(git_with(
        repository,
        args,
        hardened_environment(&safe_global_checkout_config(repository)?),
        None,
    ))
}

pub(crate) fn assert_git_clean(
    repository: &Path,
    allowed_untracked_roots: &[PathBuf],
) -> Result<()> {
    assert_no_configured_git_filters(repository)?;
    let status = run_git_status(
        repository,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !status.ok {
        return rejected("Project repository is not a valid Git repository");
    }
    for entry in split_nul(&status.stdout) {
        let path = entry
            .strip_prefix("?? ")
            .map(|untracked| resolve(&repository.join(untracked)))
            .transpose()?;
        if path.is_some_and(|path| {
            allowed_untracked_roots
                .iter()
                .any(|root| same_path(&path, root) || contained(root, &path))
        }) {
            continue;
        }
        return rejected("Project repository must be exactly clean, including untracked files");
    }
    Ok(())
}

fn assert_no_configured_git_filters(repository: &Path) -> Result<()> {
    let filters = git_with(
        repository,
        &[
            "config",
            "--local",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ],
        hardened_environment(&[]),
        None,
    );
    match filters.status {
        Some(0) if !filters.stdout.trim().is_empty() => {
            rejected("Project repository configures unsupported Git filter drivers")
        }
        Some(0 | 1) => Ok(()),
        _ => rejected("Could not inspect Project Git filter configuration"),
    }
}

pub(crate) fn capture_git_head(repository: &Path) -> Result<GitHead> {
    if !run_git(repository, &["rev-parse", "--git-dir"]).ok {
        return rejected("Project repository is not a valid Git repository");
    }
    let symbolic = run_git(repository, &["symbolic-ref", "-q", "HEAD"]);
    let commit = run_git(repository, &["rev-parse", "--verify", "HEAD"]);
    if symbolic.ok {
        let reference = symbolic.stdout.trim().to_owned();
        if !safe_git_ref(&reference) {
            return rejected("Malformed symbolic Git HEAD");
        }
        if !commit.ok {
            return Ok(GitHead {
                version: 1,
                state: "unborn".into(),
                reference: Some(reference),
                commit: None,
            });
        }
        let commit = commit.stdout.trim().to_owned();
        if !safe_git_object_id(&commit) {
            return rejected("Malformed symbolic Git HEAD commit");
        }
        return Ok(GitHead {
            version: 1,
            state: "symbolic".into(),
            reference: Some(reference),
            commit: Some(commit),
        });
    }
    let hash = commit.stdout.trim().to_owned();
    if !commit.ok || !safe_git_object_id(&hash) {
        return rejected("Malformed detached Git HEAD");
    }
    Ok(GitHead {
        version: 1,
        state: "detached".into(),
        reference: None,
        commit: Some(hash),
    })
}

pub(crate) fn read_git_head(root: &Path) -> Result<GitHead> {
    let path = root.join("git-head.json");
    if !contained(root, &path)
        || !std::fs::symlink_metadata(&path).is_ok_and(|stat| stat.is_file() && !stat.is_symlink())
    {
        return rejected("Backup Git HEAD metadata is missing, symlinked, or not a file");
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let malformed = || Error::Rejected("Malformed Git HEAD metadata".into());
    let reference = match &value["ref"] {
        Value::Null => None,
        Value::String(r) if safe_git_ref(r) => Some(r.clone()),
        _ => return Err(malformed()),
    };
    let commit = match &value["commit"] {
        Value::Null => None,
        Value::String(c) if safe_git_object_id(c) => Some(c.clone()),
        _ => return Err(malformed()),
    };
    let state = value["state"]
        .as_str()
        .filter(|s| matches!(*s, "symbolic" | "detached" | "unborn"))
        .ok_or_else(malformed)?;
    if value["version"] != 1 {
        return Err(malformed());
    }
    let consistent = match state {
        "symbolic" => reference.is_some() && commit.is_some(),
        "unborn" => reference.is_some() && commit.is_none(),
        _ => reference.is_none() && commit.is_some(),
    };
    if !consistent {
        return rejected("Inconsistent Git HEAD metadata");
    }
    Ok(GitHead {
        version: 1,
        state: state.into(),
        reference,
        commit,
    })
}

pub(crate) fn assert_git_head_consistency(
    head: &GitHead,
    refs: &BTreeMap<String, String>,
    label: &str,
) -> Result<()> {
    match head.state.as_str() {
        "symbolic" => match (&head.reference, &head.commit) {
            (Some(reference), Some(commit)) if refs.get(reference) == Some(commit) => Ok(()),
            _ => rejected(format!(
                "{label} symbolic ref does not resolve to its recorded commit"
            )),
        },
        "unborn" => match (&head.reference, &head.commit) {
            (Some(reference), None) if !refs.contains_key(reference) => Ok(()),
            _ => rejected(format!("{label} unborn ref is not absent")),
        },
        _ if head.reference.is_some() || head.commit.is_none() => {
            rejected(format!("{label} detached HEAD is malformed"))
        }
        _ => Ok(()),
    }
}

/// `for-each-ref` output as ref → object, refusing malformed or duplicate lines.
pub(crate) fn refs_from(input: &str) -> Result<BTreeMap<String, String>> {
    let mut refs = BTreeMap::new();
    for line in input
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let valid = parts.len() == 2
            && safe_git_object_id(parts[0])
            && safe_git_ref(parts[1])
            && !refs.contains_key(parts[1]);
        if !valid {
            return rejected(format!("Malformed Git ref: {line}"));
        }
        refs.insert(parts[1].to_owned(), parts[0].to_owned());
    }
    Ok(refs)
}

pub(crate) fn git_refs(repository: &Path, label: &str) -> Result<BTreeMap<String, String>> {
    let result = run_git(
        repository,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    if !result.ok {
        return rejected(format!("{label} failed"));
    }
    refs_from(&result.stdout)
}

pub(crate) fn safe_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn safe_git_ref(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("refs/") else {
        return false;
    };
    let first_ok = rest
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    first_ok
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with('/')
        && !value.split('/').any(|part| part == ".")
}

pub(crate) fn split_nul(value: &str) -> Vec<&str> {
    value.split('\0').filter(|part| !part.is_empty()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub mode: String,
    pub object_id: String,
    pub path: String,
}

pub(crate) fn git_tree_entries(repository: &Path, commit: &str) -> Result<Vec<TreeEntry>> {
    if !safe_git_object_id(commit) {
        return rejected("Git tree commit is malformed");
    }
    let listed = run_git(repository, &["ls-tree", "-r", "-z", "--full-tree", commit]);
    if !listed.ok {
        return rejected("Could not enumerate Git tree");
    }
    let mut paths = HashSet::new();
    let mut entries = Vec::new();
    for line in split_nul(&listed.stdout) {
        let unsafe_entry =
            || Error::Rejected("Git tree contains an unsupported or unsafe entry".into());
        let (header, path) = line.split_once('\t').ok_or_else(unsafe_entry)?;
        let fields: Vec<&str> = header.split(' ').collect();
        let [mode, kind, object_id] = fields[..] else {
            return Err(unsafe_entry());
        };
        if kind != "blob"
            || !matches!(mode, "100644" | "100755")
            || !safe_git_object_id(object_id)
            || checked_worktree_path(repository, path).is_err()
            || !paths.insert(path.to_owned())
        {
            return Err(unsafe_entry());
        }
        entries.push(TreeEntry {
            mode: mode.into(),
            object_id: object_id.into(),
            path: path.into(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// A repository-relative path that may be written: no `.git`, drive, `..`, or control characters.
pub(crate) fn checked_worktree_path(repository: &Path, path: &str) -> Result<PathBuf> {
    let normalized = path.replace('\\', "/");
    let unsafe_path = normalized.is_empty()
        || normalized.contains('\0')
        || normalized.contains(':')
        || normalized.contains(['\r', '\n'])
        || normalized.starts_with('/')
        || normalized.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.to_lowercase() == ".git"
        });
    if unsafe_path {
        return rejected("Git worktree path is unsafe");
    }
    let target = resolve(&repository.join(&normalized))?;
    if !contained(repository, &target) {
        return rejected("Git worktree path escapes the repository");
    }
    Ok(target)
}

pub(crate) fn assert_required_commits(
    repository: &Path,
    commits: &[String],
    label: &str,
) -> Result<()> {
    if commits.is_empty() {
        return Ok(());
    }
    let result = run_git_input(
        repository,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        &format!("{}\n", commits.join("\n")),
    );
    let observed: Vec<&str> = result
        .stdout
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect();
    let matches = observed.len() == commits.len()
        && observed
            .iter()
            .zip(commits)
            .all(|(line, commit)| *line == format!("{commit} commit"));
    if !result.ok || !matches {
        return rejected(format!("{label} are unavailable or have an invalid type"));
    }
    Ok(())
}

pub(crate) fn read_required_commits(root: &Path) -> Result<Vec<String>> {
    let path = assert_regular_contained_file(root, "required-commits.txt", "Backup Git metadata")?;
    let commits: Vec<String> = std::fs::read_to_string(path)?
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if commits.iter().collect::<HashSet<_>>().len() != commits.len() {
        return rejected("Backup required Git commits are duplicated");
    }
    if let Some(bad) = commits.iter().find(|commit| !safe_git_object_id(commit)) {
        return rejected(format!("Malformed required Git commit: {bad}"));
    }
    Ok(commits)
}

pub(crate) fn verify_bundle(bundle: &Path, required_commits: &[String]) -> Result<()> {
    let format = bundle_object_format(bundle, required_commits)?;
    let scratch = tempfile::Builder::new()
        .prefix("nosh-bundle-verify-")
        .tempdir()?;
    let absolute = resolve(bundle)?.to_string_lossy().into_owned();
    if !run_git(
        scratch.path(),
        &["init", "--bare", &format!("--object-format={format}")],
    )
    .ok || !run_git(scratch.path(), &["bundle", "verify", &absolute]).ok
    {
        return rejected("Backup Git bundle verification failed");
    }
    if !required_commits.is_empty() {
        if !run_git(scratch.path(), &["bundle", "unbundle", &absolute]).ok {
            return rejected("Backup Git bundle verification import failed");
        }
        assert_required_commits(
            scratch.path(),
            required_commits,
            "Backup Git bundle commits",
        )?;
    }
    Ok(())
}

/// The refs and commits a bundle restores, read back from a scratch repository.
fn snapshot_bundle(bundle: &Path) -> Result<(String, String)> {
    let heads = bundle_heads(bundle)?;
    let hashes: Vec<String> = heads.iter().map(|(hash, _)| hash.clone()).collect();
    verify_bundle(bundle, &hashes)?;
    let listed: Vec<String> = heads
        .iter()
        .filter(|(_, reference)| reference != "HEAD")
        .map(|(hash, reference)| format!("{hash} {reference}"))
        .collect();
    let refs = refs_from(&listed.join("\n"))?;
    let format = bundle_object_format(bundle, &hashes)?;
    let scratch = tempfile::Builder::new()
        .prefix("nosh-bundle-snapshot-")
        .tempdir()?;
    let absolute = resolve(bundle)?.to_string_lossy().into_owned();
    if !run_git(
        scratch.path(),
        &["init", "--bare", &format!("--object-format={format}")],
    )
    .ok || !run_git(scratch.path(), &["bundle", "unbundle", &absolute]).ok
    {
        return rejected("Backup Git bundle snapshot import failed");
    }
    for (reference, hash) in &refs {
        if !run_git(scratch.path(), &["update-ref", reference, hash]).ok {
            return rejected("Backup Git bundle ref materialization failed");
        }
    }
    let materialized = run_git(
        scratch.path(),
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    let commits = run_git(scratch.path(), &["rev-list", "--all"]);
    if !materialized.ok || !commits.ok {
        return rejected("Backup Git bundle snapshot enumeration failed");
    }
    Ok((materialized.stdout, commits.stdout))
}

fn bundle_heads(bundle: &Path) -> Result<Vec<(String, String)>> {
    let mut command = hidden("git");
    command
        .arg("bundle")
        .arg("list-heads")
        .arg(resolve(bundle)?)
        .env_clear()
        .envs(hardened_environment(&[]));
    let heads = git_output(&mut command, None);
    if !heads.ok {
        return rejected("Backup Git bundle head listing failed");
    }
    heads
        .stdout
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts[..] {
                [hash, reference]
                    if safe_git_object_id(hash)
                        && (reference == "HEAD" || safe_git_ref(reference)) =>
                {
                    Ok((hash.to_owned(), reference.to_owned()))
                }
                _ => rejected("Backup Git bundle head is malformed"),
            }
        })
        .collect()
}

fn bundle_object_format(bundle: &Path, object_ids: &[String]) -> Result<&'static str> {
    let lengths: HashSet<usize> = object_ids
        .iter()
        .map(String::len)
        .chain(bundle_heads(bundle)?.iter().map(|(hash, _)| hash.len()))
        .collect();
    match lengths.into_iter().collect::<Vec<_>>()[..] {
        [64] => Ok("sha256"),
        [40] => Ok("sha1"),
        _ => rejected("Backup Git bundle object format is missing or inconsistent"),
    }
}
