//! `backup` and `backupRestore` in `main.ts`: through the daemon when it runs, else directly.

use super::config::Config;
use super::http::encode_uri_component;
use super::{Cli, out};
use nosh_store::backup::RegisteredProject;
use nosh_wire::js;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;

impl Cli {
    /// The registered Project `selector` names: its id, or a path whose native realpath is its
    /// repository root (so symlinked or differently cased paths find the same Project).
    fn select_project(&self, config: &Config, selector: &str) -> Result<Value, String> {
        let resolved = nosh_sys::paths::resolve(Path::new(selector)).ok();
        let path = resolved
            .filter(|path| path.exists())
            .and_then(|path| nosh_sys::paths::realpath_native(&path).ok())
            .map(|path| path.to_string_lossy().into_owned());
        self.projects(config)?
            .into_iter()
            .find(|entry| {
                entry["projectId"] == selector
                    || path
                        .as_deref()
                        .is_some_and(|path| entry["repositoryRoot"] == path)
            })
            .ok_or_else(|| "Project is not registered".into())
    }

    /// `backup(config, selector)`.
    pub(super) fn backup(&self, config: &Config, selector: &str) -> Result<(), String> {
        let project = self.select_project(config, selector)?;
        let path = if self.healthy(config) {
            let body = json!({ "projectId": project.get("projectId") }).to_string();
            let created = self.api(config, Method::POST, "/backups", Some(body))?;
            js::string(created.get("backup").and_then(|backup| backup.get("path")))
        } else {
            nosh_store::backup::create_project_backup(
                &config.data_directory()?,
                &registered(project)?,
            )
            .map_err(|error| error.to_string())?
            .path
        };
        out(&format!("Backup written to {path}\n"));
        Ok(())
    }

    /// `backupRestore(config, selector, backupId)`: a running daemon schedules it and shuts down to
    /// apply it; otherwise the next start applies it.
    pub(super) fn backup_restore(
        &self,
        config: &Config,
        selector: &str,
        backup_id: &str,
    ) -> Result<(), String> {
        let project = self.select_project(config, selector)?;
        let id = js::string(project.get("projectId"));
        if self.healthy(config) {
            let body = json!({ "projectId": project.get("projectId") }).to_string();
            self.api(
                config,
                Method::POST,
                &format!("/backups/{}/restore", encode_uri_component(backup_id)),
                Some(body),
            )?;
            out(&format!(
                "Restore scheduled for {id}; noshd is shutting down for restart.\n"
            ));
            return Ok(());
        }
        nosh_store::restore::schedule_project_restore(
            &config.data_directory()?,
            &registered(project)?,
            backup_id,
        )
        .map_err(|error| error.to_string())?;
        out(&format!(
            "Restore scheduled for {id}; run nosh start to apply it.\n"
        ));
        Ok(())
    }
}

/// A registry row as the store's `RegisteredProject`.
fn registered(project: Value) -> Result<RegisteredProject, String> {
    serde_json::from_value(project).map_err(|error| error.to_string())
}
