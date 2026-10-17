//! The host-wide project registry, as `HostRegistry` in `packages/persistence/src/host-registry.ts`.

use crate::database::open_database;
use crate::{Result, rejected};
use nosh_sys::paths::{realpath, resolve};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

pub struct HostRegistry {
    connection: Connection,
}

impl HostRegistry {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(HostRegistry {
            connection: open_database(path)?,
        })
    }

    /// Registers (or re-points) a project. Paths are stored the way Node's `realpathSync(resolve())`
    /// and `resolve()` produce them, so either implementation finds the other's rows.
    pub fn register(
        &self,
        project_id: &str,
        repository_root: &Path,
        database_path: &Path,
    ) -> Result<Value> {
        let repository_root = realpath(repository_root)?;
        if !std::fs::metadata(&repository_root)?.is_dir() {
            return rejected("Project repository root must be a directory");
        }
        let root = repository_root.to_string_lossy().into_owned();
        let database = resolve(database_path)?.to_string_lossy().into_owned();
        let registered_at = nosh_wire::now_utc();
        self.connection.execute(
            "INSERT INTO registered_projects (project_id, repository_root, database_path, registered_at) VALUES (?, ?, ?, ?)
         ON CONFLICT(project_id) DO UPDATE SET repository_root = excluded.repository_root, database_path = excluded.database_path, registered_at = excluded.registered_at",
            params![project_id, root, database, registered_at],
        )?;
        Ok(
            json!({ "projectId": project_id, "repositoryRoot": root, "databasePath": database, "registeredAt": registered_at }),
        )
    }

    /// Registered projects, oldest registration first.
    pub fn list(&self) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare("SELECT project_id, repository_root, database_path, registered_at FROM registered_projects ORDER BY registered_at")?;
        let rows = statement.query_map([], |row| {
            Ok(json!({ "projectId": row.get::<_, String>(0)?, "repositoryRoot": row.get::<_, String>(1)?, "databasePath": row.get::<_, String>(2)?, "registeredAt": row.get::<_, String>(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_lists_with_node_style_paths() {
        let dir = tempfile::tempdir().unwrap();
        let registry = HostRegistry::open(&dir.path().join("host.sqlite")).unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let dotted = dir.path().join("repo").join("..").join("repo").join("");
        let registered = registry
            .register("prj_1", &dotted, Path::new("x/../db.sqlite"))
            .unwrap();
        assert_eq!(
            registered["repositoryRoot"],
            repo.to_string_lossy().as_ref()
        );
        let expected_db = std::env::current_dir().unwrap().join("db.sqlite");
        assert_eq!(
            registered["databasePath"],
            expected_db.to_string_lossy().as_ref()
        );
        assert_eq!(registry.list().unwrap(), vec![registered]);

        let file = dir.path().join("file.txt");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            registry
                .register("prj_2", &file, Path::new("db2.sqlite"))
                .unwrap_err()
                .to_string(),
            "Project repository root must be a directory"
        );
        assert!(
            registry
                .register(
                    "prj_3",
                    &dir.path().join("missing"),
                    Path::new("db3.sqlite")
                )
                .is_err()
        );
    }
}
