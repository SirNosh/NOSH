//! Research Projects, as in `packages/evidence/src/project.ts`.

use crate::paper::initialize_paper_workspace;
use crate::{Error, Result, regex, rejected};
use nosh_sys::paths::{realpath_native, resolve};
use nosh_wire::{IdPrefix, canonical_json, create_id, now_utc, sha256, validate_record};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const PROJECT_CONTRACT: &str = "https://nosh.dev/schemas/project-contract/v1";
const PROJECT_ROOT: &str = "https://nosh.dev/schemas/project-root/v1";
const SCHEMA_LOCK: &str = "https://nosh.dev/schemas/schema-lock/v1";

/// NOSH's runtime state lives under `.nosh` but is not research content: ignore all of it except
/// the control files that belong in Git.
pub const NOSH_GITIGNORE: &str = "# NOSH runtime state (rewritten by NOSH; ignores itself). Only the Project's control files are tracked.\n/*\n!/project.json\n!/schema-lock.json\n!/contracts/\n";

/// Executables a supervised Job can launch directly (argv, no shell).
const DIRECT_EXECUTABLES: [&str; 12] = [
    "node", "python", "python3", "py", "pytest", "bun", "deno", "go", "cargo", "make", "Rscript",
    "julia",
];

pub struct ProjectInitialization<'a> {
    pub path: &'a Path,
    pub data_directory: &'a Path,
    pub create_repository: bool,
    pub working_title: Option<&'a str>,
    pub github_repository_url: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializedProject {
    pub project_id: String,
    pub repository_root: PathBuf,
    pub database_path: PathBuf,
}

/// Candidate test and evaluation commands for discovery to propose (best effort, read only).
pub fn detect_runnable_commands(repository_root: &Path) -> Vec<Value> {
    let read = |name: &str| std::fs::read_to_string(repository_root.join(name)).ok();
    let mut found: Vec<Value> = Vec::new();
    let package = read("package.json").unwrap_or_else(|| "{}".into());
    if let Ok(scripts) = ordered_scripts(&package) {
        for (name, script) in scripts {
            let named =
                regex!(r"^(test|eval|evaluate|bench|benchmark|reproduce)(:[A-Za-z0-9_-]+)?$")
                    .is_match(&name);
            let Some(script) = script.as_str().filter(|s| {
                named && !s.contains(['&', '|', ';', '<', '>', '`', '$', '(', ')', '*'])
            }) else {
                continue;
            };
            let argv: Vec<&str> = js_split_whitespace(crate::episodes::js_trim(script));
            if !argv
                .first()
                .is_some_and(|first| DIRECT_EXECUTABLES.contains(first))
            {
                continue;
            }
            let id = regex!(r"[^a-z0-9]+")
                .replace_all(&name.to_lowercase(), "-")
                .into_owned();
            found.push(json!({ "commandId": format!("command_{id}"), "description": format!("package.json script \"{name}\""), "argv": argv, "timeoutSeconds": 600 }));
        }
    }
    let python_tests = std::fs::read_dir(repository_root.join("tests")).is_ok_and(|entries| {
        entries
            .filter_map(|e| e.ok())
            .any(|e| regex!(r"^test_.*\.py$").is_match(&e.file_name().to_string_lossy()))
    });
    if repository_root.join("pytest.ini").exists()
        || read("pyproject.toml").is_some_and(|text| text.contains("[tool.pytest"))
        || python_tests
    {
        found.push(json!({ "commandId": "command_pytest", "description": "pytest suite", "argv": ["python", "-m", "pytest", "-q"], "timeoutSeconds": 1800 }));
    }
    let makefile = read("Makefile").unwrap_or_default();
    for target in ["test", "eval", "evaluate", "reproduce"] {
        if makefile
            .split('\n')
            .any(|line| line.starts_with(&format!("{target}:")))
        {
            found.push(json!({ "commandId": format!("command_make-{target}"), "description": format!("make {target}"), "argv": ["make", target], "timeoutSeconds": 1800 }));
        }
    }
    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|command| seen.insert(command["commandId"].as_str().unwrap_or_default().to_owned()))
        .take(10)
        .collect()
}

/// `package.json`'s `scripts` entries in file order (`Object.entries(JSON.parse(text).scripts ?? {})`;
/// `serde_json` maps would sort the keys). A non-object `scripts` has no entries.
fn ordered_scripts(text: &str) -> serde_json::Result<Vec<(String, Value)>> {
    use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
    use std::fmt;

    struct Entries(Vec<(String, Value)>);
    impl<'de> Deserialize<'de> for Entries {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct EntriesVisitor;
            impl<'de> Visitor<'de> for EntriesVisitor {
                type Value = Entries;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("any JSON value")
                }
                fn visit_map<A: MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Entries, A::Error> {
                    let mut entries: Vec<(String, Value)> = Vec::new();
                    while let Some((key, value)) = map.next_entry::<String, Value>()? {
                        // JSON.parse: a repeated key keeps its first position and its last value.
                        match entries.iter_mut().find(|(existing, _)| *existing == key) {
                            Some(entry) => entry.1 = value,
                            None => entries.push((key, value)),
                        }
                    }
                    Ok(Entries(entries))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> std::result::Result<Entries, A::Error> {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    Ok(Entries(Vec::new()))
                }
                fn visit_str<E>(self, _: &str) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
                fn visit_bool<E>(self, _: bool) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
                fn visit_i64<E>(self, _: i64) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
                fn visit_u64<E>(self, _: u64) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
                fn visit_f64<E>(self, _: f64) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
                fn visit_unit<E>(self) -> std::result::Result<Entries, E> {
                    Ok(Entries(Vec::new()))
                }
            }
            deserializer.deserialize_any(EntriesVisitor)
        }
    }

    struct Package(Vec<(String, Value)>);
    impl<'de> Deserialize<'de> for Package {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct PackageVisitor;
            impl<'de> Visitor<'de> for PackageVisitor {
                type Value = Package;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a package.json object")
                }
                fn visit_map<A: MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Package, A::Error> {
                    let mut scripts = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if key == "scripts" {
                            scripts = map.next_value::<Entries>()?.0;
                        } else {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                    Ok(Package(scripts))
                }
            }
            deserializer.deserialize_map(PackageVisitor)
        }
    }

    Ok(serde_json::from_str::<Package>(text)?.0)
}

/// `value.split(/\s+/)` after trim (no empty parts for a trimmed non-empty value).
fn js_split_whitespace(value: &str) -> Vec<&str> {
    if value.is_empty() {
        return vec![""];
    }
    value
        .split(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
        .filter(|part| !part.is_empty())
        .collect()
}

/// Writes `.nosh/.gitignore` when `.nosh` exists and the file differs.
pub fn ensure_nosh_ignore(repository_root: &Path) -> Result<()> {
    let path = repository_root.join(".nosh").join(".gitignore");
    if !repository_root.join(".nosh").exists() {
        return Ok(());
    }
    if std::fs::read_to_string(&path).ok().as_deref() != Some(NOSH_GITIGNORE) {
        std::fs::write(&path, NOSH_GITIGNORE)?;
    }
    Ok(())
}

/// Creates (or re-opens) a Project in an existing or new Git repository: `.nosh` control files,
/// a draft contract, the paper workspace, and the Project database location.
pub fn initialize_research_project(
    input: &ProjectInitialization<'_>,
) -> Result<InitializedProject> {
    let requested = resolve(input.path)?;
    if input.create_repository {
        std::fs::create_dir_all(&requested)?;
        if !git(&requested, &["init"]).0 {
            return rejected("Git repository initialization failed");
        }
    }
    // The native realpath expands Windows 8.3 short names, which Git never reports.
    let root = realpath_native(&requested)?;
    if !root.is_dir() {
        return rejected("Project path must be a directory");
    }
    let (top_ok, top) = git(&root, &["rev-parse", "--show-toplevel"]);
    if !top_ok || realpath_native(Path::new(top.trim())).ok().as_deref() != Some(root.as_path()) {
        return rejected("Project path must be the root of a Git repository");
    }
    configure_github_remote(&root, input.github_repository_url)?;
    let nosh = root.join(".nosh");
    for directory in ["contracts", "events", "artifacts", "sessions", "jobs"] {
        std::fs::create_dir_all(nosh.join(directory))?;
    }
    ensure_nosh_ignore(&root)?;
    let metadata_path = nosh.join("project.json");
    let prior: Value = if metadata_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&metadata_path)?)?
    } else {
        json!({})
    };
    let project_id = prior
        .get("projectId")
        .and_then(Value::as_str)
        .map_or_else(|| create_id(IdPrefix::Prj), str::to_owned);
    let now = now_utc();
    let branch = git(&root, &["branch", "--show-current"])
        .1
        .trim()
        .to_owned();
    let branch = if branch.is_empty() {
        "main".to_owned()
    } else {
        branch
    };
    let base_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let working_title = input
        .working_title
        .map(crate::episodes::js_trim)
        .filter(|t| !t.is_empty())
        .map_or(base_name, str::to_owned);
    initialize_paper_workspace(&root, &working_title)?;
    let contract = validate_record(
        PROJECT_CONTRACT,
        &json!({ "$schema": PROJECT_CONTRACT, "schemaVersion": 1, "templateVersion": "1.0.0", "projectId": project_id, "contractVersion": 1, "workingTitle": working_title, "domainTags": [],
            "northStar": { "goalId": "goal_project", "question": "Pending collaborative Project discovery", "contributionType": "contribution_pending", "decisionUse": "Pending collaborative Project discovery" },
            "scope": { "included": [], "excluded": [] }, "datasets": [], "licensingConstraints": [], "computeEnvelope": { "maximumGpuHours": 0, "maximumDiskBytes": 0, "allowedHardwareClasses": ["local"] },
            "reproducibilityStandard": { "minimumSeeds": 1, "environmentLockRequired": true, "immutableEvaluatedCommitRequired": true, "rawLogsRetained": true },
            "paper": { "intendedVenue": null, "requiredSections": ["Abstract", "Introduction", "Related work", "Method", "Experiments", "Limitations", "Conclusion"], "claimPolicy": "evidence_link_required" },
            "policies": { "network": "network_user.approved", "privacy": "privacy_local.first", "publication": "publication_user.approved", "protectedPaths": [".git", ".nosh"] },
            "canonicalDefaultBranch": branch, "createdBy": "user", "createdAt": now, "approvedAt": null }),
    )?;
    let database_path = resolve(input.data_directory)?
        .join("projects")
        .join(&project_id)
        .join("nosh.sqlite");
    write_atomic(&nosh.join("contracts").join("project.v1.json"), &contract)?;
    let project_root = validate_record(
        PROJECT_ROOT,
        &json!({ "$schema": PROJECT_ROOT, "schemaVersion": 1, "projectId": project_id, "activeProjectContractVersion": 1, "activeProjectContractPath": ".nosh/contracts/project.v1.json", "operationalDatabaseId": "database_operational", "defaultBranch": branch, "latestPersistentSequence": 0, "updatedAt": now }),
    )?;
    write_atomic(&metadata_path, &project_root)?;
    let lock = validate_record(
        SCHEMA_LOCK,
        &json!({ "$schema": SCHEMA_LOCK, "schemaVersion": 1, "noshVersion": "0.1.0", "wirePackageVersion": "0.1.0", "templateBundleVersion": "1.0.0", "schemaDigest": sha256(&json!(nosh_wire::schema_uris())), "migrationsApplied": ["migration_1"], "updatedAt": now }),
    )?;
    write_atomic(&nosh.join("schema-lock.json"), &lock)?;
    Ok(InitializedProject {
        project_id,
        repository_root: root,
        database_path,
    })
}

/// The active contract named by `.nosh/project.json`, validated.
pub fn read_project_contract(repository_root: &Path) -> Result<Value> {
    let metadata_path = repository_root.join(".nosh").join("project.json");
    let metadata = read_project_json(&metadata_path, "metadata")?;
    let root = validate_record(PROJECT_ROOT, &metadata).map_err(|issues| {
        project_read_error(&metadata_path, "metadata", &issue_summary(&issues))
    })?;
    let active = root["activeProjectContractPath"]
        .as_str()
        .unwrap_or_default();
    // Repository-controlled metadata must not redirect reads outside .nosh/contracts.
    if !regex!(r"^\.nosh/contracts/project\.v[0-9]+\.json$").is_match(active) {
        return Err(project_read_error(
            &metadata_path,
            "metadata",
            "activeProjectContractPath must name .nosh/contracts/project.v<N>.json",
        ));
    }
    // Node's `join` turns the separators into `\`; `Path::join` would keep the `/`.
    let contract_path = active
        .split('/')
        .fold(repository_root.to_path_buf(), |path, part| path.join(part));
    let contract = read_project_json(&contract_path, "active contract")?;
    validate_record(PROJECT_CONTRACT, &contract).map_err(|issues| {
        project_read_error(&contract_path, "active contract", &issue_summary(&issues))
    })
}

fn read_project_json(path: &Path, label: &str) -> Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(project_read_error(path, label, "file is missing"));
        }
        Err(_) => return Err(project_read_error(path, label, "file could not be read")),
    };
    // Parser messages quote file content; report only the failure class.
    serde_json::from_str(&text)
        .map_err(|_| project_read_error(path, label, "file is not valid JSON"))
}

fn project_read_error(path: &Path, label: &str, detail: &str) -> Error {
    Error::Rejected(format!(
        "Cannot load Project {label} at \"{}\": {detail}. Repair this file to match its NOSH schema, then retry. No older contract was selected.",
        path.display()
    ))
}

/// The first three issues as `a.b.c: message` (zod's issue order may differ from Rust's).
fn issue_summary(issues: &[nosh_wire::Issue]) -> String {
    let path = |pointer: &str| {
        let joined = pointer
            .split('/')
            .skip(1)
            .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
            .collect::<Vec<_>>()
            .join(".");
        if joined.is_empty() {
            "(root)".to_owned()
        } else {
            joined
        }
    };
    let summary = issues
        .iter()
        .take(3)
        .map(|issue| format!("{}: {}", path(&issue.pointer), issue.message))
        .collect::<Vec<_>>()
        .join("; ");
    if issues.len() > 3 {
        format!("{summary}; +{} more validation issues", issues.len() - 3)
    } else {
        summary
    }
}

/// Commits exactly the active contract and NOSH's scaffolding (never other staged work, never a
/// push); best effort: `{ committed, detail }`.
pub fn commit_project_contract(repository_root: &Path) -> Result<Value> {
    let contract = read_project_contract(repository_root)?;
    let trust = format!(
        "safe.directory={}",
        nosh_sys::paths::forward_slashes(repository_root)
    );
    let run = |args: &[&str]| {
        let mut command = nosh_sys::process::hidden("git");
        command
            .arg("-C")
            .arg(repository_root)
            .args(["-c", &trust])
            .args(args);
        nosh_sys::process::output_with_timeout(&mut command, Duration::from_secs(20)).ok()
    };
    let ok = |output: &Option<(Option<std::process::ExitStatus>, std::process::Output)>| {
        output
            .as_ref()
            .is_some_and(|(status, _)| status.is_some_and(|s| s.success()))
    };
    let stdout = |output: &Option<(Option<std::process::ExitStatus>, std::process::Output)>| {
        output
            .as_ref()
            .map(|(_, o)| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    if !ok(&run(&["rev-parse", "--is-inside-work-tree"])) {
        return Ok(json!({ "committed": false, "detail": "not a Git repository" }));
    }
    let contracts_dir = repository_root.join(".nosh").join("contracts");
    let mut contracts: Vec<String> = std::fs::read_dir(&contracts_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| regex!(r"^project\.v[0-9]+\.json$").is_match(n))
                .map(|n| format!(".nosh/contracts/{n}"))
                .collect()
        })
        .unwrap_or_default();
    contracts.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let untracked = |path: &str| {
        repository_root.join(path).exists()
            && !ok(&run(&["ls-files", "--error-unmatch", "--", path]))
    };
    let version = nosh_wire::js::string(contract.get("contractVersion"));
    let mut paths: Vec<String> = vec![
        ".nosh/project.json".into(),
        format!(".nosh/contracts/project.v{version}.json"),
    ];
    paths.extend(contracts);
    for path in [".nosh/schema-lock.json", "docs/paper.md", "docs/paper.bib"] {
        let include = if path.starts_with(".nosh/") {
            repository_root.join(path).exists()
        } else {
            untracked(path)
        };
        if include {
            paths.push(path.into());
        }
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    let path_args: Vec<&str> = paths.iter().map(String::as_str).collect();
    if !ok(&run(&[&["add", "--"][..], &path_args].concat())) {
        return Ok(
            json!({ "committed": false, "detail": "the contract files are ignored or unreadable by Git" }),
        );
    }
    if stdout(&run(
        &[&["status", "--porcelain", "--"][..], &path_args].concat()
    ))
    .trim()
    .is_empty()
    {
        return Ok(json!({ "committed": false, "detail": "already committed" }));
    }
    let identity: &[&str] = if ok(&run(&["config", "user.email"])) {
        &[]
    } else {
        &[
            "-c",
            "user.name=NOSH",
            "-c",
            "user.email=nosh@localhost.invalid",
        ]
    };
    let message = format!("NOSH: approve project contract v{version}");
    let result = run(&[identity, &["commit", "-m", &message, "--"], &path_args].concat());
    if ok(&result) {
        return Ok(
            json!({ "committed": true, "detail": stdout(&run(&["rev-parse", "HEAD"])).trim() }),
        );
    }
    let detail = result
        .as_ref()
        .map(|(_, o)| {
            if o.stderr.is_empty() {
                String::from_utf8_lossy(&o.stdout).into_owned()
            } else {
                String::from_utf8_lossy(&o.stderr).into_owned()
            }
        })
        .unwrap_or_default();
    Ok(json!({ "committed": false, "detail": nosh_wire::js::slice_to(detail.trim(), 300) }))
}

/// Approves the draft contract (discovery's last step).
pub fn approve_project_contract(repository_root: &Path, input: &Value) -> Result<Value> {
    let current = read_project_contract(repository_root)?;
    if nosh_wire::js::truthy(current.get("approvedAt")) {
        return rejected("Project discovery is already complete");
    }
    write_approved_successor(repository_root, &current, input)
}

/// A user's amendment of an approved contract.
pub fn amend_project_contract(repository_root: &Path, input: &Value) -> Result<Value> {
    let current = read_project_contract(repository_root)?;
    if !nosh_wire::js::truthy(current.get("approvedAt")) {
        return rejected("Complete Project discovery before amending the contract");
    }
    write_approved_successor(repository_root, &current, input)
}

fn write_approved_successor(
    repository_root: &Path,
    current: &Value,
    input: &Value,
) -> Result<Value> {
    // The host records the approval moment; agents must not invent approval metadata.
    let mut candidate = input.as_object().cloned().unwrap_or_default();
    candidate.insert("approvedAt".into(), json!(now_utc()));
    let contract = validate_record(PROJECT_CONTRACT, &Value::Object(candidate))?;
    let same = |key: &str| nosh_wire::js::strict_equal(contract.get(key), current.get(key));
    let next_version = current["contractVersion"].as_f64().map(|v| v + 1.0);
    if !same("projectId")
        || contract["contractVersion"].as_f64() != next_version
        || !same("createdAt")
        || !same("createdBy")
        || !same("canonicalDefaultBranch")
    {
        return rejected("Project contract identity or version does not match the draft");
    }
    let root_path = repository_root.join(".nosh").join("project.json");
    let root = validate_record(
        PROJECT_ROOT,
        &serde_json::from_str(&std::fs::read_to_string(&root_path)?)?,
    )?;
    let version = nosh_wire::js::string(contract.get("contractVersion"));
    let contract_path = format!(".nosh/contracts/project.v{version}.json");
    write_atomic(&repository_root.join(&contract_path), &contract)?;
    let mut updated = root.as_object().cloned().unwrap_or_default();
    updated.insert(
        "activeProjectContractVersion".into(),
        contract["contractVersion"].clone(),
    );
    updated.insert("activeProjectContractPath".into(), json!(contract_path));
    updated.insert("updatedAt".into(), json!(now_utc()));
    write_atomic(
        &root_path,
        &validate_record(PROJECT_ROOT, &Value::Object(updated))?,
    )?;
    Ok(contract)
}

/// `git -C <root> <args>` with a 10 s timeout: `(succeeded, stdout)`.
fn git(root: &Path, args: &[&str]) -> (bool, String) {
    let mut command = nosh_sys::process::hidden("git");
    command.arg("-C").arg(root).args(args).stdin(Stdio::null());
    match nosh_sys::process::output_with_timeout(&mut command, Duration::from_secs(10)) {
        Ok((status, output)) => (
            status.is_some_and(|s| s.success()),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ),
        Err(_) => (false, String::new()),
    }
}

fn write_atomic(path: &Path, value: &Value) -> Result<()> {
    std::fs::create_dir_all(path.parent().expect("files live in a directory"))?;
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&temporary, format!("{}\n", canonical_json(value)))?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn configure_github_remote(root: &Path, value: Option<&str>) -> Result<()> {
    let Some(url) = value
        .map(crate::episodes::js_trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(());
    };
    if !regex!(r"^https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:\.git)?/?$").is_match(url) {
        return rejected(
            "GitHub repository must be a full https://github.com/owner/repository URL",
        );
    }
    let (has_origin, origin) = git(root, &["remote", "get-url", "origin"]);
    if has_origin && origin.trim() != url {
        return rejected("Existing Git origin does not match the GitHub repository link");
    }
    if !has_origin && !git(root, &["remote", "add", "origin", url]).0 {
        return rejected("GitHub repository could not be configured as origin");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_top_level_scripts_object_in_file_order() {
        let text = r#"{"name":"x","nested":{"scripts":{"no":"x"}},"scripts":{"test":"node t.js","eval:fast":"python e.py","build":"tsc"}}"#;
        let names: Vec<String> = ordered_scripts(text)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["test", "eval:fast", "build"]);
        assert!(ordered_scripts("{}").unwrap().is_empty());
    }
}
