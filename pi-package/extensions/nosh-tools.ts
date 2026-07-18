import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const tools = [
  ["nosh_task_acknowledge", "Accept or reject the daemon-issued task packet."],
  ["nosh_progress_emit", "Emit a durable milestone, anomaly, warning, or blocker."],
  ["nosh_response_submit", "Submit a terminal worker or director response."],
  ["nosh_review_submit", "Submit an independent criterion-level review verdict."],
  ["nosh_blocker_submit", "Submit an externally actionable blocker and resume predicate."],
  ["nosh_graph_change_propose", "Propose a versioned graph change for daemon validation."],
  ["nosh_delegation_request", "Request, but do not directly spawn, a bounded worker."],
  ["nosh_handoff_create", "Create a deterministic ownership handoff."],
  ["nosh_handoff_teachback", "Acknowledge and validate a received handoff."],
  ["nosh_experiment_propose", "Propose a non-duplicate experiment under the frozen contract."],
  ["nosh_evidence_submit", "Register evidence linked to exact artifacts or runs."],
  ["nosh_episode_submit", "Submit the compact typed episode for the current logical thread step."],
  ["nosh_runtime_instruct", "Execute one authorized typed orchestration instruction."],
] as const;

export default function noshTools(pi: ExtensionAPI) {
  for (const [name, description] of tools) {
    pi.registerTool(defineTool({
      name,
      label: name,
      description,
      parameters: Type.Object({ record: Type.Unknown({ description: "Complete NOSH schema record" }) }),
      async execute(_toolCallId, parameters) {
        const base = process.env.NOSH_DAEMON_URL;
        const token = process.env.NOSH_BOOTSTRAP_TOKEN;
        if (!base || !token) throw new Error("NOSH daemon connection is not configured");
        const response = await fetch(`${base.replace(/\/$/, "")}/records`, {
          method: "POST",
          headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
          body: JSON.stringify({
            tool: name,
            projectId: process.env.NOSH_PROJECT_ID,
            attemptKey: process.env.NOSH_ATTEMPT_KEY,
            record: parameters.record,
          }),
        });
        const result = await response.json() as { accepted?: boolean; retryAllowed?: boolean; errors?: unknown };
        return {
          content: [{ type: "text", text: response.ok ? "NOSH accepted the typed record." : `NOSH rejected the record: ${JSON.stringify(result.errors ?? result)}` }],
          details: { ...result, httpStatus: response.status },
          isError: !response.ok,
        };
      },
    }));
  }
}
