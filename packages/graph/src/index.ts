export type NodeState = "pending" | "ready" | "leased" | "working" | "postflight" | "reviewing" | "accepted" | "blocked" | "failed" | "waived" | "superseded" | "cancelled";
export type GraphNode = {
  id: string; type: string; title: string; required: boolean; hardDependencies: string[]; softDependencies: string[];
  state: NodeState; attempt: number; maximumAttempts: number; priority: number; criticalWeight: number; createdAt: string;
  lease: { leaseId: string; ownerId: string; version: number; expiresAt: string } | null;
};
export type GraphOperation =
  | { type: "add_node"; node: GraphNode }
  | { type: "add_hard_edge"; from: string; to: string }
  | { type: "remove_hard_edge"; from: string; to: string }
  | { type: "supersede_node"; nodeId: string; replacementId: string | null };
export type GraphVersion = { version: number; nodes: GraphNode[]; rationale: string; evidenceIds: string[]; createdAt: string };

const transitions: Record<NodeState, NodeState[]> = {
  pending: ["ready", "cancelled", "superseded"], ready: ["leased", "blocked", "cancelled", "superseded"], leased: ["working", "ready", "blocked", "cancelled"],
  working: ["postflight", "blocked", "failed", "cancelled"], postflight: ["reviewing", "working", "failed"], reviewing: ["accepted", "working", "blocked", "failed"],
  accepted: [], blocked: ["ready", "cancelled", "superseded"], failed: ["ready", "superseded", "cancelled"], waived: [], superseded: [], cancelled: [],
};

export class VersionedDag {
  private readonly history: GraphVersion[];

  constructor(readonly scopeId: string, nodes: GraphNode[], rationale = "initial plan", initialVersion = 1) {
    validate(nodes);
    if (!Number.isInteger(initialVersion) || initialVersion < 1) throw new Error("Initial graph version must be positive");
    this.history = [{ version: initialVersion, nodes: clone(nodes), rationale, evidenceIds: [], createdAt: new Date().toISOString() }];
    this.refreshReady();
  }

  current(): GraphVersion { return cloneVersion(this.history.at(-1)!); }
  versions(): GraphVersion[] { return this.history.map(cloneVersion); }

  apply(baseVersion: number, operations: GraphOperation[], rationale: string, evidenceIds: string[]): GraphVersion {
    const current = this.current();
    if (baseVersion !== current.version) throw new Error(`Stale graph version ${baseVersion}; current version is ${current.version}`);
    const nodes = current.nodes;
    for (const operation of operations) {
      if (operation.type === "add_node") {
        if (nodes.some((node) => node.id === operation.node.id)) throw new Error(`Node ${operation.node.id} already exists`);
        nodes.push(clone([operation.node])[0]!);
      } else if (operation.type === "add_hard_edge") {
        required(nodes, operation.from); const target = required(nodes, operation.to);
        if (!target.hardDependencies.includes(operation.from)) target.hardDependencies.push(operation.from);
      } else if (operation.type === "remove_hard_edge") {
        const target = required(nodes, operation.to);
        if (target.lease) throw new Error("Cannot rewire an actively leased node");
        target.hardDependencies = target.hardDependencies.filter((id) => id !== operation.from);
      } else {
        const node = required(nodes, operation.nodeId);
        if (node.lease) throw new Error("Cannot supersede an actively leased node");
        node.state = "superseded";
        if (operation.replacementId) required(nodes, operation.replacementId);
      }
    }
    validate(nodes);
    const version = { version: current.version + 1, nodes, rationale, evidenceIds: [...evidenceIds], createdAt: new Date().toISOString() };
    this.history.push(version); this.refreshReady(); return this.current();
  }

  transition(nodeId: string, next: NodeState): GraphNode {
    const version = this.history.at(-1)!; const node = required(version.nodes, nodeId);
    if (!transitions[node.state].includes(next)) throw new Error(`Illegal node transition ${node.state} -> ${next}`);
    if (next === "leased" && node.hardDependencies.some((id) => required(version.nodes, id).state !== "accepted" && required(version.nodes, id).state !== "waived")) throw new Error("Hard dependencies are not satisfied");
    if (next === "ready" && node.attempt >= node.maximumAttempts) throw new Error("Node attempt budget is exhausted");
    if (next === "working") node.attempt += 1;
    if (["ready", "accepted", "blocked", "failed", "cancelled"].includes(next)) node.lease = null;
    node.state = next; this.refreshReady(); return clone([node])[0]!;
  }

  lease(nodeId: string, lease: NonNullable<GraphNode["lease"]>): GraphNode {
    const version = this.history.at(-1)!; const node = required(version.nodes, nodeId);
    if (node.state !== "ready" || node.lease) throw new Error("Only an unleased ready node can be leased");
    node.lease = { ...lease }; node.state = "leased"; return clone([node])[0]!;
  }

  releaseExpired(now = Date.now()): string[] {
    const released: string[] = [];
    for (const node of this.history.at(-1)!.nodes) if (node.lease && Date.parse(node.lease.expiresAt) <= now) { node.lease = null; node.state = "ready"; released.push(node.id); }
    return released;
  }

  runnable(): GraphNode[] { return this.current().nodes.filter((node) => node.state === "ready" && !node.lease); }
  completionReady(): boolean { return this.current().nodes.every((node) => !node.required || node.state === "accepted" || node.state === "waived" || node.state === "superseded"); }
  criticalPath(): string[] {
    const nodes = this.current().nodes; const scores = new Map<string, { score: number; path: string[] }>();
    for (const id of topological(nodes)) { const node = required(nodes, id); const parent = node.hardDependencies.map((dependency) => scores.get(dependency)!).sort((a, b) => b.score - a.score)[0]; scores.set(id, { score: (parent?.score ?? 0) + node.criticalWeight, path: [...(parent?.path ?? []), id] }); }
    return [...scores.values()].sort((a, b) => b.score - a.score)[0]?.path ?? [];
  }

  diff(fromVersion: number, toVersion: number): { added: string[]; superseded: string[]; rewired: string[] } {
    const from = this.history.find((version) => version.version === fromVersion); const to = this.history.find((version) => version.version === toVersion);
    if (!from || !to) throw new Error("Unknown graph version");
    const added = to.nodes.filter((node) => !from.nodes.some((prior) => prior.id === node.id)).map((node) => node.id);
    const superseded = to.nodes.filter((node) => node.state === "superseded" && from.nodes.find((prior) => prior.id === node.id)?.state !== "superseded").map((node) => node.id);
    const rewired = to.nodes.filter((node) => { const prior = from.nodes.find((candidate) => candidate.id === node.id); return prior && JSON.stringify([...prior.hardDependencies].sort()) !== JSON.stringify([...node.hardDependencies].sort()); }).map((node) => node.id);
    return { added, superseded, rewired };
  }

  private refreshReady(): void {
    const nodes = this.history.at(-1)!.nodes;
    for (const node of nodes) if (node.state === "pending" && node.hardDependencies.every((id) => ["accepted", "waived"].includes(required(nodes, id).state))) node.state = "ready";
  }
}

function validate(nodes: GraphNode[]): void {
  const ids = new Set<string>();
  for (const node of nodes) { if (ids.has(node.id)) throw new Error(`Duplicate node ${node.id}`); ids.add(node.id); }
  for (const node of nodes) for (const dependency of node.hardDependencies) if (!ids.has(dependency)) throw new Error(`Unknown dependency ${dependency}`);
  topological(nodes);
}
function topological(nodes: GraphNode[]): string[] {
  const visiting = new Set<string>(); const visited = new Set<string>(); const result: string[] = [];
  const visit = (id: string) => { if (visiting.has(id)) throw new Error("Scheduling graph contains a cycle"); if (visited.has(id)) return; visiting.add(id); for (const dependency of required(nodes, id).hardDependencies) visit(dependency); visiting.delete(id); visited.add(id); result.push(id); };
  nodes.forEach((node) => visit(node.id)); return result;
}
function required(nodes: GraphNode[], id: string): GraphNode { const node = nodes.find((candidate) => candidate.id === id); if (!node) throw new Error(`Unknown node ${id}`); return node; }
function clone(nodes: GraphNode[]): GraphNode[] { return nodes.map((node) => ({ ...node, hardDependencies: [...node.hardDependencies], softDependencies: [...node.softDependencies], lease: node.lease ? { ...node.lease } : null })); }
function cloneVersion(version: GraphVersion): GraphVersion { return { ...version, nodes: clone(version.nodes), evidenceIds: [...version.evidenceIds] }; }

export * from "./research.js";
