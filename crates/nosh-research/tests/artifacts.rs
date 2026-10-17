//! Artifact store tests: the TypeScript tests ported, then one store shared between the
//! TypeScript `ArtifactStore` (`scripts/ts-call.mjs`) and this crate.

use nosh_research::artifacts::{ArtifactInput, ArtifactStore, append_jsonl, recover_jsonl};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn input(id: &str, source: &Path, class: &str) -> ArtifactInput {
    json!({ "artifactId": id, "projectId": "prj_1", "kind": "evaluation", "mediaType": "application/json", "sourcePath": source, "retentionClass": class }).as_object().unwrap().clone()
}

fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

/// Port of "addresses artifacts by bytes and retains negative evidence".
#[test]
fn addresses_artifacts_by_bytes_and_retains_negative_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("result.json");
    std::fs::write(&source, "{\"negative\":true}\n").unwrap();
    let store = ArtifactStore::open(&dir.path().join("store")).unwrap();
    let artifact = store
        .add(&input("art_1", &source, "negative_evidence"))
        .unwrap();
    assert_eq!(
        store.resolve("art_1", None).unwrap()["contentHash"],
        artifact["contentHash"]
    );
    assert_eq!(
        store
            .add(&input("art_1", &source, "negative_evidence"))
            .unwrap()["version"],
        artifact["version"]
    );
    assert_eq!(store.cleanup(&["negative_evidence"]).unwrap().len(), 1);
    std::fs::write(&source, "{\"negative\":false}\n").unwrap();
    assert_eq!(
        store
            .add(&input("art_1", &source, "negative_evidence"))
            .unwrap()["version"],
        2
    );
    assert_eq!(store.resolve("art_1", Some(1)).unwrap()["version"], 1);

    let hash = artifact["contentHash"]
        .as_str()
        .unwrap()
        .trim_start_matches("sha256:");
    let object = dir
        .path()
        .join("store")
        .join(&hash[..2])
        .join(format!("{hash}.json"));
    assert_eq!(artifact["storedPath"], json!(object));
    std::fs::write(&object, "tampered").unwrap();
    assert_eq!(
        store.resolve("art_1", Some(1)).unwrap_err().to_string(),
        "Artifact art_1 does not resolve to its stored hash"
    );
    assert_eq!(
        store.resolve("art_missing", None).unwrap_err().to_string(),
        "Artifact art_missing does not resolve to its stored hash"
    );
}

/// Port of "repairs a partial trailing JSONL line".
#[test]
fn repairs_a_partial_trailing_jsonl_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.jsonl");
    append_jsonl(&path, &json!({ "id": 1 })).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    assert_eq!(recover_jsonl(&path).unwrap(), vec![json!({ "id": 1 })]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"id\":1}\n");
}

/// The TypeScript version is skipped on Windows (symlinks need a privilege); a junction needs none.
#[test]
fn rejects_sources_and_roots_behind_junctions() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("source.json"), "{}\n").unwrap();
    junction(&dir.path().join("linked"), &real);
    let store = ArtifactStore::open(&dir.path().join("store")).unwrap();
    let error = store
        .add(&input(
            "art_linked",
            &dir.path().join("linked/source.json"),
            "negative_evidence",
        ))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Artifact source traverses a symlink or reparse point"
    );
    let error = ArtifactStore::open(&dir.path().join("linked"))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(error, "Artifact path traverses a symlink or reparse point");
}

fn typescript(root: &Path, calls: Value) -> Vec<Value> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let request = json!({ "module": "packages/evidence/dist/index.js", "construct": { "name": "ArtifactStore", "args": [root] }, "calls": calls });
    let mut child = Command::new("node")
        .arg(repo.join("scripts/ts-call.mjs"))
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

/// One store, written alternately by both implementations: each sees the other's versions, dedupes
/// against the other's content hashes, keeps the other's fields, and refuses the same junction.
#[test]
fn one_store_shared_with_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let source = dir.path().join("metrics.tar.gz");
    std::fs::write(&source, "v1").unwrap();
    let rust = ArtifactStore::open(&root).unwrap();
    let mut extra = input("art_shared", &source, "checkpoint");
    extra.insert("futureField".into(), json!({ "kept": true }));
    let first = rust.add(&extra).unwrap();

    std::fs::write(dir.path().join("other.txt"), "other").unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("x.json"), "{}").unwrap();
    junction(&dir.path().join("linked"), &real);
    let theirs = typescript(
        &root,
        json!([
            { "method": "resolve", "args": ["art_shared"] },
            { "method": "add", "args": [input("art_shared", &source, "checkpoint")] },
            { "method": "add", "args": [input("art_other", &dir.path().join("other.txt"), "cache")] },
            { "method": "add", "args": [input("art_linked", &dir.path().join("linked/x.json"), "cache")] },
        ]),
    );
    assert_eq!(
        theirs[0]["ok"], first,
        "TypeScript resolves the Rust version, unknown fields included"
    );
    assert_eq!(
        theirs[1]["ok"]["version"], 1,
        "same bytes: TypeScript dedupes against the Rust hash"
    );
    assert_eq!(
        theirs[3]["error"],
        "Artifact source traverses a symlink or reparse point"
    );

    std::fs::write(&source, "v2").unwrap();
    let second = rust
        .add(&input("art_shared", &source, "checkpoint"))
        .unwrap();
    assert_eq!(second["version"], 2);
    let other = rust.resolve("art_other", None).unwrap();
    assert_eq!(
        other, theirs[2]["ok"],
        "Rust resolves the TypeScript version identically"
    );
    let listed = typescript(
        &root,
        json!([{ "method": "list" }, { "method": "resolve", "args": ["art_shared", 2] }, { "method": "cleanup", "args": [["cache", "checkpoint"]] }]),
    );
    assert_eq!(listed[0]["ok"], Value::Array(rust.list().unwrap()));
    assert_eq!(listed[1]["ok"], second);
    assert_eq!(
        listed[2]["ok"],
        Value::Array(rust.cleanup(&["cache", "checkpoint"]).unwrap())
    );
    assert_eq!(
        rust.list().unwrap()[0]["futureField"],
        json!({ "kept": true }),
        "fields survive both implementations' rewrites"
    );
}
