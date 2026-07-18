export type ResponseCard = {
  status: "completed" | "partial" | "blocked" | "failed" | "cancelled";
  scope: "Project" | "Mission" | "Direction" | "Autoresearch" | "Node";
  outcome: string; produced: string[]; validation: string[]; issues: string[]; next: string[];
};

export function renderResponseMarkdown(card: ResponseCard): string {
  return [
    `Status: ${title(card.status)}`, `Scope: ${card.scope}`, "", "Outcome", card.outcome, "", "Produced or changed", bullets(card.produced, "None"), "",
    "Validation and review", bullets(card.validation, "Pending"), "", "Risks, defects, and blockers", bullets(card.issues, "None"), "", "Next", bullets(card.next, "No action required"), "",
  ].join("\n");
}

export function renderResponseHtml(card: ResponseCard, compact = false): string {
  const section = (heading: string, items: string[], empty: string) => `<section><h3>${escape(heading)}</h3><ul>${(items.length ? items : [empty]).map((item) => `<li>${escape(item)}</li>`).join("")}</ul></section>`;
  return `<article class="nosh-response${compact ? " nosh-response--compact" : ""}"><header><b>${escape(title(card.status))}</b><span>${escape(card.scope)}</span></header><section><h3>Outcome</h3><p>${escape(card.outcome)}</p></section>${section("Produced or changed", card.produced, "None")}${section("Validation and review", card.validation, "Pending")}${section("Risks, defects, and blockers", card.issues, "None")}${section("Next", card.next, "No action required")}</article>`;
}

export function projectContractDefaults(projectId: string, title: string, northStarQuestion: string) {
  return {
    $schema: "https://nosh.dev/schemas/project-contract/v1", schemaVersion: 1, templateVersion: "1.0.0", projectId, contractVersion: 1, workingTitle: title, domainTags: [],
    northStar: { goalId: "pgoal_01", question: northStarQuestion, contributionType: "empirical_study", decisionUse: "Determine whether the evidence supports a paper contribution." },
    scope: { included: [], excluded: [] }, datasets: [], licensingConstraints: [], computeEnvelope: { maximumGpuHours: 0, maximumDiskBytes: 0, allowedHardwareClasses: ["local_wsl2_cuda"] },
    reproducibilityStandard: { minimumSeeds: 3, environmentLockRequired: true, immutableEvaluatedCommitRequired: true, rawLogsRetained: true },
    paper: { intendedVenue: null, requiredSections: ["abstract", "introduction", "related_work", "method", "experiments", "limitations", "conclusion"], claimPolicy: "evidence_link_required" },
    policies: { network: "role_scoped", privacy: "project_local_by_default", publication: "user_approval_required", protectedPaths: ["docs/paper.md", ".nosh/contracts/project.json"] },
    canonicalDefaultBranch: "main", createdBy: "user", createdAt: null, approvedAt: null,
  };
}

function bullets(items: string[], empty: string): string { return (items.length ? items : [empty]).map((item) => `- ${item}`).join("\n"); }
function title(value: string): string { return `${value[0]?.toUpperCase()}${value.slice(1)}`; }
function escape(value: string): string { return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;"); }
