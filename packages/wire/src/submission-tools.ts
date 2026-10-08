import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Record schemas each typed NOSH submission tool accepts. Single source for the gate and tool descriptions. */
export const submissionToolSchemas: Readonly<Record<string, readonly string[]>> = {
  nosh_task_acknowledge: ["task-acknowledgement"],
  nosh_progress_emit: ["progress-update"],
  nosh_response_submit: ["general-worker-completion", "librarian-completion", "task-failure", "mission-director-cycle", "research-director-cycle"],
  nosh_review_submit: ["review-verdict"],
  nosh_blocker_submit: ["blocker"],
  nosh_graph_change_propose: ["graph-change-proposal"],
  nosh_delegation_request: ["delegation-request"],
  nosh_handoff_create: ["handoff"],
  nosh_handoff_teachback: ["handoff-teachback"],
  nosh_experiment_propose: ["experiment-proposal"],
  nosh_evidence_submit: ["evidence"],
  nosh_episode_submit: ["episode-draft"],
  nosh_runtime_instruct: ["runtime-instruction"],
  nosh_project_contract_submit: ["project-contract"],
};

/** Generated JSON Schema for a record (`pnpm schemas`); shipped under packages/wire/src/schemas. */
export function schemaDocumentPath(name: string): string {
  return resolve(dirname(fileURLToPath(import.meta.url)), "..", "src", "schemas", `${name}.v1.schema.json`);
}

/** Episode type a role's THREAD_STEP must produce. Roles contain underscores, but a ref allows exactly one. */
export function episodeTypeForRole(role: string): string {
  return `episode_${role.replace(/_/g, "-")}`;
}
