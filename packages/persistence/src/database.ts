import Database from "better-sqlite3";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";

type Migration = { version: number; sql: string };

const migrations: Migration[] = [
  {
    version: 1,
    sql: `
      CREATE TABLE project_sequences (
        project_id TEXT PRIMARY KEY,
        last_sequence INTEGER NOT NULL CHECK (last_sequence >= 0)
      );

      CREATE TABLE events (
        project_id TEXT NOT NULL,
        sequence INTEGER NOT NULL CHECK (sequence > 0),
        event_id TEXT NOT NULL UNIQUE,
        event_type TEXT NOT NULL,
        timestamp TEXT NOT NULL,
        event_json TEXT NOT NULL,
        PRIMARY KEY (project_id, sequence)
      );

      CREATE TABLE command_receipts (
        project_id TEXT NOT NULL,
        idempotency_key TEXT NOT NULL,
        result_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, idempotency_key)
      );

      CREATE TABLE snapshots (
        project_id TEXT NOT NULL,
        sequence INTEGER NOT NULL CHECK (sequence >= 0),
        checksum TEXT NOT NULL,
        snapshot_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, sequence)
      );

      CREATE TABLE projection_heads (
        project_id TEXT PRIMARY KEY,
        sequence INTEGER NOT NULL CHECK (sequence >= 0),
        updated_at TEXT NOT NULL
      );

      CREATE TABLE registered_projects (
        project_id TEXT PRIMARY KEY,
        repository_root TEXT NOT NULL UNIQUE,
        database_path TEXT NOT NULL UNIQUE,
        registered_at TEXT NOT NULL
      );
    `,
  },
];

export function openDatabase(path: string): Database.Database {
  mkdirSync(dirname(path), { recursive: true });
  const database = new Database(path);
  database.pragma("journal_mode = WAL");
  database.pragma("foreign_keys = ON");
  database.exec("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)");

  for (const migration of migrations) {
    const applied = database.prepare("SELECT 1 FROM schema_migrations WHERE version = ?").get(migration.version);
    if (applied) continue;
    database.transaction(() => {
      database.exec(migration.sql);
      database.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(migration.version, new Date().toISOString());
    })();
  }

  return database;
}
