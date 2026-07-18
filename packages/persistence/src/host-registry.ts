import { nowUtc } from "@nosh/core";
import type Database from "better-sqlite3";
import { realpathSync, statSync } from "node:fs";
import { resolve } from "node:path";
import { openDatabase } from "./database.js";

export type RegisteredProject = {
  projectId: string;
  repositoryRoot: string;
  databasePath: string;
  registeredAt: string;
};

export class HostRegistry {
  readonly database: Database.Database;

  constructor(databasePath: string) {
    this.database = openDatabase(databasePath);
  }

  register(project: Omit<RegisteredProject, "registeredAt">): RegisteredProject {
    const repositoryRoot = realpathSync(resolve(project.repositoryRoot));
    if (!statSync(repositoryRoot).isDirectory()) throw new Error("Project repository root must be a directory");

    const registered: RegisteredProject = { ...project, repositoryRoot, databasePath: resolve(project.databasePath), registeredAt: nowUtc() };
    this.database
      .prepare(
        `INSERT INTO registered_projects (project_id, repository_root, database_path, registered_at) VALUES (?, ?, ?, ?)
         ON CONFLICT(project_id) DO UPDATE SET repository_root = excluded.repository_root, database_path = excluded.database_path, registered_at = excluded.registered_at`,
      )
      .run(registered.projectId, registered.repositoryRoot, registered.databasePath, registered.registeredAt);
    return registered;
  }

  list(): RegisteredProject[] {
    return this.database
      .prepare("SELECT project_id, repository_root, database_path, registered_at FROM registered_projects ORDER BY registered_at")
      .all()
      .map((row) => {
        const record = row as { project_id: string; repository_root: string; database_path: string; registered_at: string };
        return {
          projectId: record.project_id,
          repositoryRoot: record.repository_root,
          databasePath: record.database_path,
          registeredAt: record.registered_at,
        };
      });
  }

  close(): void {
    this.database.close();
  }
}
