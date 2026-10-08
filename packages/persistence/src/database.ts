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
  {
    version: 2,
    sql: `
      CREATE TABLE entity_projections (
        project_id TEXT NOT NULL,
        entity_type TEXT NOT NULL,
        entity_id TEXT NOT NULL,
        version INTEGER NOT NULL CHECK (version > 0),
        state TEXT NOT NULL,
        projection_json TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (project_id, entity_type, entity_id)
      );

      CREATE INDEX entity_projections_by_type ON entity_projections (project_id, entity_type, updated_at);

      CREATE TABLE graph_versions (
        project_id TEXT NOT NULL,
        scope_type TEXT NOT NULL,
        scope_id TEXT NOT NULL,
        version INTEGER NOT NULL CHECK (version > 0),
        graph_json TEXT NOT NULL,
        rationale TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, scope_type, scope_id, version)
      );
    `,
  },
  {
    version: 3,
    sql: `
      CREATE TABLE operation_intents (
        intent_id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL,
        operation_type TEXT NOT NULL,
        idempotency_key TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending', 'completed', 'failed')),
        request_json TEXT NOT NULL,
        result_json TEXT,
        error TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE (project_id, idempotency_key)
      );

      CREATE INDEX operation_intents_by_state ON operation_intents (project_id, state, created_at);
    `,
  },
  {
    version: 4,
    sql: `
      CREATE TABLE terminal_submissions (
        project_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        tool_name TEXT NOT NULL,
        schema_uri TEXT NOT NULL,
        record_hash TEXT NOT NULL,
        receipt_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, task_id, tool_name, schema_uri)
      );
    `,
  },
  {
    version: 5,
    sql: `
      CREATE TABLE terminal_task_tools (
        project_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        tool_name TEXT NOT NULL,
        schema_uri TEXT NOT NULL,
        record_hash TEXT NOT NULL,
        receipt_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, task_id, tool_name)
      );

      INSERT INTO terminal_task_tools (project_id, task_id, tool_name, schema_uri, record_hash, receipt_json, created_at)
      SELECT current.project_id, current.task_id, current.tool_name, current.schema_uri, current.record_hash, current.receipt_json, current.created_at
      FROM terminal_submissions AS current
      WHERE NOT EXISTS (
        SELECT 1
        FROM terminal_submissions AS prior
        WHERE prior.project_id = current.project_id
          AND prior.task_id = current.task_id
          AND prior.tool_name = current.tool_name
          AND (prior.created_at < current.created_at OR (prior.created_at = current.created_at AND prior.schema_uri < current.schema_uri))
      );
    `,
  },
  {
    version: 6,
    sql: `
      CREATE TABLE terminal_task_authority (
        project_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        tool_name TEXT NOT NULL,
        schema_uri TEXT NOT NULL,
        record_hash TEXT NOT NULL,
        receipt_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, task_id)
      );
      CREATE TABLE terminal_task_legacy_ambiguities (
        project_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        terminal_count INTEGER NOT NULL,
        recorded_at TEXT NOT NULL,
        PRIMARY KEY (project_id, task_id)
      );
      INSERT INTO terminal_task_legacy_ambiguities (project_id, task_id, terminal_count, recorded_at)
      SELECT project_id, task_id, COUNT(*), CURRENT_TIMESTAMP
      FROM terminal_task_tools
      GROUP BY project_id, task_id
      HAVING COUNT(*) > 1;
      INSERT INTO terminal_task_authority (project_id, task_id, tool_name, schema_uri, record_hash, receipt_json, created_at)
      SELECT current.project_id, current.task_id, current.tool_name, current.schema_uri, current.record_hash, current.receipt_json, current.created_at
      FROM terminal_task_tools AS current
      WHERE NOT EXISTS (
        SELECT 1 FROM terminal_task_tools AS prior
        WHERE prior.project_id = current.project_id AND prior.task_id = current.task_id
          AND (prior.created_at, prior.tool_name, prior.schema_uri, prior.record_hash) < (current.created_at, current.tool_name, current.schema_uri, current.record_hash)
      );
    `,
  },
  {
    version: 7,
    sql: `CREATE TABLE terminal_turn_receipts (
      project_id TEXT NOT NULL, turn_id TEXT NOT NULL, context_hash TEXT NOT NULL,
      receipt_json TEXT NOT NULL, PRIMARY KEY (project_id, turn_id)
    );`,
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

export async function backupDatabase(source: string, destination: string): Promise<number> {
  const database = openDatabase(source);
  try { await database.backup(destination); return (database.prepare("SELECT COALESCE(MAX(version), 0) AS version FROM schema_migrations").get() as { version: number }).version; }
  finally { database.close(); }
}
