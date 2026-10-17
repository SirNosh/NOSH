//! The content-addressed Artifact store, as `ArtifactStore` in `packages/evidence/src/index.ts`.
//!
//! Objects live at `<root>/<hash[0..2]>/<hash><ext>`; `<root>/manifest.json` lists every version
//! (stored paths relative to the root). Nothing under the root, or on the way to it, may be a
//! symbolic link or junction. Manifest entries are kept as JSON objects, so fields this build does
//! not know survive a rewrite.

use nosh_sys::paths::{extname, relative_inside, resolve};
use nosh_sys::{Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub struct ArtifactStore {
    root: PathBuf,
    manifest_path: PathBuf,
}

/// What a caller supplies for a new Artifact version (`Omit<Artifact, …>`); other fields pass through.
pub type ArtifactInput = Map<String, Value>;

impl ArtifactStore {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        assert_safe_path(root, root)?;
        Ok(ArtifactStore {
            root: root.to_path_buf(),
            manifest_path: root.join("manifest.json"),
        })
    }

    /// Stores the file at `input.sourcePath` and records it as the next version of
    /// `input.artifactId`, unless that version already holds the same content.
    pub fn add(&self, input: &ArtifactInput) -> Result<Value> {
        let source_text = input
            .get("sourcePath")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Rejected("Artifact source is not a regular file".into()))?;
        let source = resolve(Path::new(source_text))?;
        assert_unlinked_absolute(&source, "Artifact source")?;
        assert_safe_path(&self.root, &self.root)?;
        assert_regular_file(&source, "Artifact source")?;
        let bytes = std::fs::read(&source)?;
        let hash = hex_sha256(&bytes);
        let content_hash = format!("sha256:{hash}");
        let stored_relative =
            PathBuf::from(&hash[..2]).join(format!("{hash}{}", extname(&source.to_string_lossy())));
        let stored_path = self.root.join(&stored_relative);
        assert_safe_path(&self.root, &stored_path)?;
        std::fs::create_dir_all(
            stored_path
                .parent()
                .expect("objects live in a hash directory"),
        )?;
        if stored_path.exists() {
            assert_regular_file(&stored_path, "Artifact object")?;
        } else {
            std::fs::copy(&source, &stored_path)?;
        }
        let mut records = self.raw_list()?;
        let artifact_id = input.get("artifactId").cloned().unwrap_or(Value::Null);
        let prior = records
            .iter()
            .filter(|record| record["artifactId"] == artifact_id)
            .max_by(|a, b| version(a).total_cmp(&version(b)))
            .cloned();
        if let Some(prior) = &prior
            && prior["contentHash"] == content_hash.as_str()
        {
            return self.public(prior);
        }
        let mut artifact = input.clone();
        artifact.insert("sourcePath".into(), json!(source.to_string_lossy()));
        artifact.insert("contentHash".into(), json!(content_hash));
        artifact.insert("sizeBytes".into(), json!(std::fs::metadata(&source)?.len()));
        artifact.insert(
            "version".into(),
            json!(prior.as_ref().map_or(0.0, version) as i64 + 1),
        );
        artifact.insert(
            "storedPath".into(),
            json!(stored_relative.to_string_lossy()),
        );
        artifact.insert("createdAt".into(), json!(nosh_wire::now_utc()));
        let artifact = Value::Object(artifact);
        records.push(artifact.clone());
        self.save(&records)?;
        self.public(&artifact)
    }

    /// The newest (or the given) version of an Artifact, verified against its content hash.
    pub fn resolve(&self, artifact_id: &str, version_wanted: Option<i64>) -> Result<Value> {
        let unresolved = || {
            Error::Rejected(format!(
                "Artifact {artifact_id} does not resolve to its stored hash"
            ))
        };
        let artifact = self
            .raw_list()?
            .into_iter()
            .filter(|record| {
                record["artifactId"] == artifact_id
                    && version_wanted.is_none_or(|v| record["version"].as_i64() == Some(v))
            })
            .max_by(|a, b| version(a).total_cmp(&version(b)))
            .ok_or_else(unresolved)?;
        let stored = self.absolute_stored_path(&artifact)?;
        assert_regular_file(&stored, "Artifact object")?;
        if format!("sha256:{}", hex_sha256(&std::fs::read(&stored)?))
            != artifact["contentHash"].as_str().unwrap_or_default()
        {
            return Err(unresolved());
        }
        with_stored_path(&artifact, &stored)
    }

    /// The Artifacts that survive a cleanup of `retention_classes` (accepted and negative evidence always do).
    pub fn cleanup(&self, retention_classes: &[&str]) -> Result<Vec<Value>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|artifact| {
                let class = artifact["retentionClass"].as_str().unwrap_or_default();
                !retention_classes.contains(&class)
                    || class == "accepted_evidence"
                    || class == "negative_evidence"
            })
            .collect())
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        assert_safe_path(&self.root, &self.root)?;
        if self.manifest_path.exists() {
            assert_regular_file(&self.manifest_path, "Artifact manifest")?;
        }
        self.raw_list()?
            .iter()
            .map(|artifact| self.public(artifact))
            .collect()
    }

    fn raw_list(&self) -> Result<Vec<Value>> {
        if !self.manifest_path.exists() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(
            &self.manifest_path,
        )?)?)
    }

    fn absolute_stored_path(&self, artifact: &Value) -> Result<PathBuf> {
        let stored = Path::new(artifact["storedPath"].as_str().unwrap_or_default());
        let absolute = if stored.is_absolute() {
            resolve(stored)?
        } else {
            resolve(&self.root.join(stored))?
        };
        assert_safe_path(&self.root, &absolute)?;
        Ok(absolute)
    }

    fn public(&self, artifact: &Value) -> Result<Value> {
        with_stored_path(artifact, &self.absolute_stored_path(artifact)?)
    }

    fn save(&self, records: &[Value]) -> Result<()> {
        assert_safe_path(&self.root, &self.manifest_path)?;
        let mut temporary = self.manifest_path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        assert_unlinked_absolute(&temporary, "Artifact manifest temporary")?;
        if let Ok(stat) = std::fs::symlink_metadata(&temporary) {
            if stat.is_symlink() || !stat.is_file() {
                return Err(Error::Rejected(
                    "Artifact manifest temporary is not a regular file".into(),
                ));
            }
            std::fs::remove_file(&temporary)?;
        }
        let root = resolve(&self.root)?;
        let persisted = records
            .iter()
            .map(|artifact| {
                let absolute = self.absolute_stored_path(artifact)?;
                let relative = relative_inside(&root, &absolute)?
                    .ok_or_else(|| Error::Rejected("Artifact path escapes artifact root".into()))?;
                with_stored_path(artifact, &relative)
            })
            .collect::<Result<Vec<_>>>()?;
        // `flag: "wx"`: never write through a file that appeared since the check above.
        let written = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(format!("{}\n", Value::Array(persisted)).as_bytes())?;
            drop(file);
            std::fs::rename(&temporary, &self.manifest_path)
        })();
        if let Err(error) = written {
            if std::fs::symlink_metadata(&temporary).is_ok_and(|stat| stat.is_file()) {
                let _ = std::fs::remove_file(&temporary);
            }
            return Err(error.into());
        }
        Ok(())
    }
}

fn version(artifact: &Value) -> f64 {
    artifact["version"].as_f64().unwrap_or(0.0)
}

fn with_stored_path(artifact: &Value, stored: &Path) -> Result<Value> {
    let mut artifact = artifact
        .as_object()
        .cloned()
        .ok_or_else(|| Error::Rejected("Artifact manifest entry is not an object".into()))?;
    artifact.insert("storedPath".into(), json!(stored.to_string_lossy()));
    Ok(Value::Object(artifact))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

fn assert_regular_file(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(stat) if stat.is_file() && !stat.is_symlink() => Ok(()),
        _ => Err(Error::Rejected(format!("{label} is not a regular file"))),
    }
}

/// No component of `candidate`, from the drive root down, may be a symbolic link or junction.
fn assert_unlinked_absolute(candidate: &Path, label: &str) -> Result<()> {
    let target = resolve(candidate)?;
    let mut current = PathBuf::new();
    for component in target.components() {
        current.push(component);
        if std::fs::symlink_metadata(&current).is_ok_and(|stat| stat.is_symlink()) {
            return Err(Error::Rejected(format!(
                "{label} traverses a symlink or reparse point"
            )));
        }
    }
    Ok(())
}

fn assert_safe_path(root: &Path, candidate: &Path) -> Result<()> {
    if relative_inside(root, candidate)?.is_none() {
        return Err(Error::Rejected(
            "Artifact path escapes artifact root".into(),
        ));
    }
    assert_unlinked_absolute(candidate, "Artifact path")
}

/// Appends `JSON.stringify(record)` and a newline (`appendCanonicalJsonl`).
pub fn append_jsonl(path: &Path, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format!("{record}\n").as_bytes())?;
    Ok(())
}

/// Reads a JSONL file up to its first unparseable line, rewrites it without the damaged tail, and
/// returns the records (`recoverJsonl`).
pub fn recover_jsonl(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for line in text.split('\n').filter(|line| !line.is_empty()) {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) => break,
        }
    }
    let repaired: Vec<String> = records.iter().map(Value::to_string).collect();
    std::fs::write(
        path,
        if repaired.is_empty() {
            String::new()
        } else {
            format!("{}\n", repaired.join("\n"))
        },
    )?;
    Ok(records)
}
