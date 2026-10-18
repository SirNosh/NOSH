//! The Project paper (`docs/paper.md` and `docs/paper.bib`): versioned reads, crash-safe paired
//! replacement through a durable operation intent, and export.

use super::*;
use nosh_wire::sha256;
use std::path::Path;

const MAXIMUM_BYTES: usize = 2_000_000;

impl<H: Host> ResearchControl<H> {
    /// `{ markdown, bibliography, markdownHash, bibliographyHash, version }`.
    pub fn read_paper(&self, project_id: &str) -> Result<Value> {
        let docs = self.repository_root(project_id)?.join("docs");
        let markdown = read_text(&docs.join("paper.md"))?;
        let bibliography = read_text(&docs.join("paper.bib"))?;
        let (markdown_hash, bibliography_hash) =
            (sha256(&json!(markdown)), sha256(&json!(bibliography)));
        let version = sha256(
            &json!({ "markdownHash": markdown_hash, "bibliographyHash": bibliography_hash }),
        );
        Ok(
            json!({ "markdown": markdown, "bibliography": bibliography, "markdownHash": markdown_hash, "bibliographyHash": bibliography_hash, "version": version }),
        )
    }

    /// Replaces the paper pair if it is still at `expected` (`{ markdownHash, bibliographyHash,
    /// version }`); returns the `paper.saved` event.
    pub fn save_paper(
        &self,
        project_id: &str,
        markdown: &str,
        bibliography: &str,
        expected: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        self.host.assert_writable(project_id)?;
        if markdown.len() > MAXIMUM_BYTES || bibliography.len() > MAXIMUM_BYTES {
            return rejected("Paper Markdown and bibliography must each be at most 2 MB");
        }
        let store = self.store(project_id)?;
        let candidate = json!({ "markdown": markdown, "bibliography": bibliography, "markdownHash": sha256(&json!(markdown)), "bibliographyHash": sha256(&json!(bibliography)), "expected": expected, "bytes": { "markdown": markdown.len(), "bibliography": bibliography.len() } });
        let prior = self.external_operation(project_id, idempotency_key)?;
        if prior.is_none()
            && store
                .operation_intents(project_id, Some("pending"))?
                .iter()
                .any(|intent| intent["operationType"] == "paper.replace")
        {
            return rejected("A paper replacement is already pending");
        }
        if let Some(prior) = &prior
            && (prior["operationType"] != "paper.replace"
                || sha256(&prior["request"]) != sha256(&candidate))
        {
            return rejected("Idempotency key was already used for a different paper replacement");
        }
        match &prior {
            Some(prior) => {
                if let Some(replay) = self.terminal_operation_replay(project_id, prior)? {
                    return Ok(replay);
                }
            }
            None => {
                let current = self.read_paper(project_id)?;
                if ["markdownHash", "bibliographyHash", "version"]
                    .iter()
                    .any(|key| !same(current.get(*key), expected.get(*key)))
                {
                    return rejected("Paper version conflict");
                }
            }
        }
        let request = prior.map_or(candidate, |prior| prior["request"].clone());
        let intent =
            store.begin_operation(project_id, "paper.replace", idempotency_key, &request)?;
        self.finish_paper_operation(project_id, &intent)
    }

    /// Finishes the operations a crash left pending; returns how many it completed.
    pub fn reconcile_pending_operations(&self, project_id: &str) -> Result<usize> {
        let mut completed = 0;
        for intent in self
            .store(project_id)?
            .operation_intents(project_id, Some("pending"))?
        {
            match intent["operationType"].as_str() {
                Some("paper.replace") => {
                    self.finish_paper_operation(project_id, &intent)?;
                }
                Some("user.evidence.create" | "user.claim.create" | "user.claim.edit") => {
                    self.finish_user_record_operation(project_id, &intent)?;
                }
                _ => continue,
            }
            completed += 1;
        }
        Ok(completed)
    }

    pub fn export_paper(&self, project_id: &str) -> Result<Value> {
        self.host.assert_writable(project_id)?;
        let root = self.repository_root(project_id)?;
        let manifest = crate::paper::export_paper(
            &root.join("docs").join("paper.md"),
            &root.join("docs").join("paper.bib"),
            &root.join(".nosh").join("paper-export"),
        )?;
        to_json(&manifest)
    }

    fn finish_paper_operation(&self, project_id: &str, intent: &Value) -> Result<Value> {
        let request = paper_replace_request(&intent["request"])?;
        let docs = self.repository_root(project_id)?.join("docs");
        let paths = [docs.join("paper.md"), docs.join("paper.bib")];
        let contents = [
            string(request.get("markdown")),
            string(request.get("bibliography")),
        ];
        let desired = [
            string(request.get("markdownHash")),
            string(request.get("bibliographyHash")),
        ];
        let expected = [
            string(request["expected"].get("markdownHash")),
            string(request["expected"].get("bibliographyHash")),
        ];
        let operation_id = string(intent.get("intentId"));
        recover_paper_staging(&paths, &expected, &desired, &operation_id)?;
        let current = [hash_text(&paths[0])?, hash_text(&paths[1])?];
        if (0..2).any(|index| current[index] != expected[index] && current[index] != desired[index])
        {
            return rejected("Paper targets changed outside the stored replacement intent");
        }
        if current != desired {
            replace_paper_pair(&paths, &contents, &desired, &operation_id)?;
        }
        if [hash_text(&paths[0])?, hash_text(&paths[1])?] != desired {
            return rejected("Paper pair replacement verification failed");
        }
        cleanup_paper_staging(&paths, &operation_id);
        let version =
            sha256(&json!({ "markdownHash": desired[0], "bibliographyHash": desired[1] }));
        let store = self.store(project_id)?;
        let draft = json!({ "$schema": schema_uri("event"), "schemaVersion": 1, "retention": "persistent", "type": "paper.saved", "source": "noshd", "scope": project_scope(project_id), "correlationId": null, "causationId": null, "payload": { "markdownHash": desired[0], "bibliographyHash": desired[1], "version": version, "bytes": request["bytes"] } });
        let stored = store.append_idempotent(&string(intent.get("idempotencyKey")), &draft)?;
        let event = stored.receipt["event"].clone();
        store.complete_operation(project_id, &operation_id, &json!({ "eventId": event["eventId"], "markdownHash": desired[0], "bibliographyHash": desired[1], "version": version }))?;
        if !stored.replayed {
            self.host.publish(&event);
        }
        Ok(event)
    }
}

/// The stored replacement request, checked for internal consistency.
fn paper_replace_request(value: &Value) -> Result<Value> {
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let expected = &value["expected"];
    let bytes = &value["bytes"];
    let consistent = match (
        text("markdown"),
        text("bibliography"),
        text("markdownHash"),
        text("bibliographyHash"),
    ) {
        (Some(markdown), Some(bibliography), Some(markdown_hash), Some(bibliography_hash)) => {
            expected.is_object()
                && [
                    expected.get("markdownHash"),
                    expected.get("bibliographyHash"),
                    expected.get("version"),
                ]
                .iter()
                .all(|field| is_str(*field))
                && bytes.is_object()
                && number(bytes.get("markdown")).is_some()
                && number(bytes.get("bibliography")).is_some()
                && sha256(&json!(markdown)) == markdown_hash
                && sha256(&json!(bibliography)) == bibliography_hash
                && number(bytes.get("markdown")) == Some(markdown.len() as f64)
                && number(bytes.get("bibliography")) == Some(bibliography.len() as f64)
                && is(
                    expected.get("version"),
                    &sha256(
                        &json!({ "markdownHash": expected["markdownHash"], "bibliographyHash": expected["bibliographyHash"] }),
                    ),
                )
        }
        _ => false,
    };
    if !consistent {
        return rejected("Paper operation intent is corrupt");
    }
    Ok(
        json!({ "markdown": value["markdown"], "bibliography": value["bibliography"], "markdownHash": value["markdownHash"], "bibliographyHash": value["bibliographyHash"], "expected": { "markdownHash": expected["markdownHash"], "bibliographyHash": expected["bibliographyHash"], "version": expected["version"] }, "bytes": { "markdown": bytes["markdown"], "bibliography": bytes["bibliography"] } }),
    )
}

/// The file's text, or "" when it does not exist (`readText`).
fn read_text(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

fn hash_text(path: &Path) -> Result<String> {
    Ok(sha256(&json!(read_text(path)?)))
}

fn suffixed(path: &Path, suffix: String) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

/// `(staged, previous)` paths of a replacement.
fn paper_paths(paths: &[PathBuf; 2], operation_id: &str) -> ([PathBuf; 2], [PathBuf; 2]) {
    let staged = [
        suffixed(&paths[0], format!(".{operation_id}.0.tmp")),
        suffixed(&paths[1], format!(".{operation_id}.1.tmp")),
    ];
    let previous = [
        suffixed(&paths[0], format!(".{operation_id}.0.rollback")),
        suffixed(&paths[1], format!(".{operation_id}.1.rollback")),
    ];
    (staged, previous)
}

/// `rmSync(path, { force: true })`.
fn remove_forced(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn recover_paper_staging(
    paths: &[PathBuf; 2],
    expected: &[String; 2],
    desired: &[String; 2],
    operation_id: &str,
) -> Result<()> {
    let (staged, previous) = paper_paths(paths, operation_id);
    for path in &staged {
        remove_forced(path)?;
    }
    for index in 0..2 {
        if !previous[index].exists() {
            continue;
        }
        if !paths[index].exists() {
            std::fs::rename(&previous[index], &paths[index])?;
        } else {
            let target = hash_text(&paths[index])?;
            if target != expected[index] && target != desired[index] {
                return rejected("Paper replacement recovery found an inconsistent rollback pair");
            }
            remove_forced(&previous[index])?;
        }
    }
    Ok(())
}

fn cleanup_paper_staging(paths: &[PathBuf; 2], operation_id: &str) {
    let (staged, previous) = paper_paths(paths, operation_id);
    for path in staged.iter().chain(&previous) {
        if let Err(error) = remove_forced(path) {
            eprintln!("nosh paper cleanup retained {}: {error}", path.display());
        }
    }
}

fn replace_paper_pair(
    paths: &[PathBuf; 2],
    contents: &[String; 2],
    desired: &[String; 2],
    operation_id: &str,
) -> Result<()> {
    let (staged, previous) = paper_paths(paths, operation_id);
    let mut moved = [false, false];
    let mut installed = [false, false];
    let attempt = (|| -> Result<()> {
        for index in 0..2 {
            if let Some(parent) = paths[index].parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&staged[index], &contents[index])?;
        }
        for index in 0..2 {
            if paths[index].exists() {
                std::fs::rename(&paths[index], &previous[index])?;
                moved[index] = true;
            }
        }
        for index in 0..2 {
            std::fs::rename(&staged[index], &paths[index])?;
            installed[index] = true;
        }
        if [hash_text(&paths[0])?, hash_text(&paths[1])?] != *desired {
            return rejected("Paper pair staging verification failed");
        }
        Ok(())
    })();
    if let Err(error) = attempt {
        for index in 0..2 {
            remove_forced(&staged[index])?;
            if installed[index] {
                remove_forced(&paths[index])?;
            }
            if moved[index] && previous[index].exists() {
                std::fs::rename(&previous[index], &paths[index])?;
            }
        }
        return Err(error);
    }
    cleanup_paper_staging(paths, operation_id);
    Ok(())
}
