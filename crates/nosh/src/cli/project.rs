//! `projectOpen`, `projectList`, `missionList`, `missionStatus`, `jobList` and `projects` in
//! `main.ts`.

use super::config::Config;
use super::http::encode_uri_component;
use super::{Cli, out};
use nosh_research::project::{ProjectInitialization, initialize_research_project};
use nosh_store::HostRegistry;
use nosh_wire::js;
use reqwest::Method;
use serde_json::Value;
use std::path::Path;

const NO_PROJECT: &str = "No current Project; run `nosh project open <path>`";

/// `value?.[key]` along `keys`, JavaScript's `undefined` being `None`.
fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(value, |value, key| value.get(key))
}

impl Cli {
    /// `projects(config)`: the daemon's registry, else (on any failure) the host registry itself,
    /// which opening creates.
    pub(super) fn projects(&self, config: &Config) -> Result<Vec<Value>, String> {
        if let Ok(reply) = self.api(config, Method::GET, "/projects", None) {
            return Ok(reply
                .get("projects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default());
        }
        let registry = HostRegistry::open(&config.data_directory()?.join("host.sqlite"))
            .map_err(|error| error.to_string())?;
        registry.list().map_err(|error| error.to_string())
    }

    /// `projectOpen(config, input)`: the CLI initializes the Project files itself, then registers
    /// it with the daemon or, when none runs, in the host registry.
    pub(super) fn project_open(&self, config: &Config, input: &str) -> Result<(), String> {
        let resolved =
            nosh_sys::paths::resolve(Path::new(input)).map_err(|error| error.to_string())?;
        // Node's `realpathSync.native` error, which Rust's io error would not match.
        if !resolved.exists() {
            return Err(format!(
                "ENOENT: no such file or directory, realpath '{}'",
                resolved.display()
            ));
        }
        let data_directory = config.data_directory()?;
        let project = initialize_research_project(&ProjectInitialization {
            path: Path::new(input),
            data_directory: &data_directory,
            create_repository: false,
            working_title: None,
            github_repository_url: None,
        })
        .map_err(|error| error.to_string())?;
        let root = project.repository_root.to_string_lossy();
        let database = project.database_path.to_string_lossy();
        if self.healthy(config) {
            // `JSON.stringify(registration)`, keys in its order.
            let body = format!(
                "{{\"projectId\":{},\"repositoryRoot\":{},\"databasePath\":{}}}",
                Value::from(project.project_id.as_str()),
                Value::from(root.as_ref()),
                Value::from(database.as_ref())
            );
            self.api(config, Method::POST, "/projects", Some(body))?;
        } else {
            HostRegistry::open(&data_directory.join("host.sqlite"))
                .and_then(|registry| {
                    registry.register(
                        &project.project_id,
                        &project.repository_root,
                        &project.database_path,
                    )
                })
                .map_err(|error| error.to_string())?;
        }
        self.save_config(
            &config.with("currentProjectId", Value::from(project.project_id.as_str())),
        )?;
        out(&format!(
            "Opened {}\nPaper: {}\n",
            project.project_id,
            project
                .repository_root
                .join("docs")
                .join("paper.md")
                .display()
        ));
        Ok(())
    }

    /// `projectList(config)`, oldest registration first; `*` marks the current Project.
    pub(super) fn project_list(&self, config: &Config) -> Result<(), String> {
        let projects = self.projects(config)?;
        if projects.is_empty() {
            out("No Projects registered\n");
        }
        for project in &projects {
            let current =
                js::strict_equal(project.get("projectId"), config.get("currentProjectId"));
            out(&format!(
                "{}{}\t{}\n",
                js::string(project.get("projectId")),
                if current { " *" } else { "" },
                js::string(project.get("repositoryRoot"))
            ));
        }
        Ok(())
    }

    /// `missionList(config)`.
    pub(super) fn mission_list(&self, config: &Config) -> Result<(), String> {
        let project = config.current_project_id().ok_or(NO_PROJECT)?;
        let reply = self.api(
            config,
            Method::GET,
            &format!("/missions?projectId={}", encode_uri_component(&project)),
            None,
        )?;
        let missions = reply
            .get("missions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if missions.is_empty() {
            out("No Missions recorded for the current Project\n");
        }
        for mission in &missions {
            out(&format!(
                "{}\t{}\tversion {}\n",
                js::string(mission.get("entityId")),
                js::string(mission.get("state")),
                js::string(mission.get("version"))
            ));
        }
        Ok(())
    }

    /// `missionStatus(config, missionId)`.
    pub(super) fn mission_status(&self, config: &Config, mission_id: &str) -> Result<(), String> {
        let project = config.current_project_id().ok_or(NO_PROJECT)?;
        let reply = self.api(
            config,
            Method::GET,
            &format!(
                "/missions/{}?projectId={}",
                encode_uri_component(mission_id),
                encode_uri_component(&project)
            ),
            None,
        )?;
        let show = |keys: &[&str]| js::string(field(&reply, keys));
        out(&format!(
            "{}\nstate: {}\nversion: {}\ntitle: {}\ngraphVersion: {}\nupdatedAt: {}\n",
            show(&["mission", "entityId"]),
            show(&["mission", "state"]),
            show(&["mission", "version"]),
            show(&["mission", "value", "title"]),
            show(&["mission", "value", "graphVersion"]),
            show(&["mission", "value", "updatedAt"])
        ));
        Ok(())
    }

    /// `jobList(config)`: every Job the daemon knows, with no offline fallback.
    pub(super) fn job_list(&self, config: &Config) -> Result<(), String> {
        let reply = self.api(config, Method::GET, "/jobs", None)?;
        let jobs = reply
            .get("jobs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if jobs.is_empty() {
            out("No jobs\n");
        }
        for job in &jobs {
            out(&format!(
                "{}\t{}\t{}\n",
                js::string(job.get("jobId")),
                js::string(job.get("state")),
                js::string(job.get("projectId"))
            ));
        }
        Ok(())
    }
}
