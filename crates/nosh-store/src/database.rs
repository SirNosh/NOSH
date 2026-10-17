//! Opening a NOSH SQLite database, as `openDatabase` in `packages/persistence/src/database.ts`.

use crate::migrations::MIGRATIONS;
use crate::{Result, rejected};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// The newest schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].0;

/// Opens (creating if needed) the database at `path` with WAL and foreign keys, and applies any
/// missing migrations, each in its own transaction. Refuses a database migrated by a newer NOSH,
/// whose tables this build would misread.
pub fn open_database(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    // journal_mode reports the resulting mode as a row.
    connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)")?;

    let newest: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if newest > SCHEMA_VERSION {
        return rejected(format!(
            "{} has schema version {newest}, newer than this NOSH supports ({SCHEMA_VERSION}); update NOSH to open it",
            path.display()
        ));
    }
    for &(version, sql) in MIGRATIONS {
        let applied = connection
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?",
                [version],
                |_| Ok(()),
            )
            .optional()?;
        if applied.is_some() {
            continue;
        }
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)",
            params![version, nosh_wire::now_utc()],
        )?;
        transaction.commit()?;
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_root() -> std::path::PathBuf {
        std::env::var_os("NOSH_TS_ROOT").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
            Into::into,
        )
    }

    /// Our copy of every migration must equal the TypeScript one, character for character.
    #[test]
    fn matches_typescript() {
        let path = ts_root().join("packages/persistence/src/database.ts");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n");
        let theirs: Vec<&str> = source
            .split("sql: `")
            .skip(1)
            .map(|rest| rest.split('`').next().expect("closing backtick"))
            .collect();
        let ours: Vec<&str> = MIGRATIONS.iter().map(|(_, sql)| *sql).collect();
        assert_eq!(ours, theirs);
        let versions: Vec<i64> = MIGRATIONS.iter().map(|(v, _)| *v).collect();
        assert_eq!(versions, (1..=SCHEMA_VERSION).collect::<Vec<_>>());
    }

    #[test]
    fn migrates_once_and_refuses_newer_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/project.sqlite");
        let versions = |connection: &Connection| -> Vec<i64> {
            let mut statement = connection
                .prepare("SELECT version FROM schema_migrations ORDER BY version")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(|v| v.unwrap())
                .collect()
        };
        let first = open_database(&path).unwrap();
        assert_eq!(versions(&first), (1..=SCHEMA_VERSION).collect::<Vec<_>>());
        let mode: String = first
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(first);
        assert_eq!(
            versions(&open_database(&path).unwrap()),
            (1..=SCHEMA_VERSION).collect::<Vec<_>>()
        );

        let future = Connection::open(&path).unwrap();
        future
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)",
                params![SCHEMA_VERSION + 1, "2030-01-01T00:00:00.000Z"],
            )
            .unwrap();
        drop(future);
        let error = open_database(&path).unwrap_err().to_string();
        assert!(
            error.contains("schema version 8, newer than this NOSH supports (7)"),
            "{error}"
        );
    }

    /// Port of "migrates legacy schema-specific terminal rows to one deterministic task-tool authority row".
    #[test]
    fn migrates_legacy_terminal_rows_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL); CREATE TABLE terminal_submissions (project_id TEXT NOT NULL, task_id TEXT NOT NULL, tool_name TEXT NOT NULL, schema_uri TEXT NOT NULL, record_hash TEXT NOT NULL, receipt_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (project_id, task_id, tool_name, schema_uri));").unwrap();
        for version in 1..=4 {
            legacy
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)",
                    params![version, "2025-01-01T00:00:00.000Z"],
                )
                .unwrap();
        }
        let insert = "INSERT INTO terminal_submissions VALUES (?, ?, ?, ?, ?, ?, ?)";
        legacy
            .execute(
                insert,
                params![
                    "prj_1",
                    "tsk_1",
                    "nosh_response_submit",
                    "https://nosh.dev/schemas/librarian-completion/v1",
                    "sha256:later",
                    "{}",
                    "2025-01-02T00:00:00.000Z"
                ],
            )
            .unwrap();
        legacy
            .execute(
                insert,
                params![
                    "prj_1",
                    "tsk_1",
                    "nosh_response_submit",
                    "https://nosh.dev/schemas/general-worker-completion/v1",
                    "sha256:earlier",
                    "{}",
                    "2025-01-01T00:00:00.000Z"
                ],
            )
            .unwrap();
        drop(legacy);
        let store = open_database(&path).unwrap();
        let row: (String, String) = store
            .query_row("SELECT schema_uri, record_hash FROM terminal_task_tools WHERE project_id = ? AND task_id = ? AND tool_name = ?", ["prj_1", "tsk_1", "nosh_response_submit"], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!(
            row,
            (
                "https://nosh.dev/schemas/general-worker-completion/v1".into(),
                "sha256:earlier".into()
            )
        );
    }
}
