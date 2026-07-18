import { orchestrationProgramSchema, runtimeInstructionSchema, skillManifestSchema, type ExecutionThread, type JsonValue, type OrchestrationProgram, type SkillManifest } from "@nosh/wire";

export class SkillRegistry {
  private readonly skills = new Map<string, SkillManifest>();
  add(input: unknown): SkillManifest { const skill = skillManifestSchema.parse(input); this.skills.set(skill.skillId, skill); return skill; }
  get(skillId: string): SkillManifest { const skill = this.skills.get(skillId); if (!skill) throw new Error(`Unknown skill ${skillId}`); return skill; }
  list(): SkillManifest[] { return [...this.skills.values()]; }
  applicable(skillId: string, thread: ExecutionThread): SkillManifest {
    const skill = this.get(skillId);
    if (!skill.activation.roles.includes(thread.role)) throw new Error(`Skill ${skillId} is not allowed for role ${thread.role}`);
    const missing = skill.activation.requiredCapabilities.filter((capability) => !thread.capabilities.includes(capability));
    if (missing.length) throw new Error(`Skill ${skillId} requires missing capabilities: ${missing.join(", ")}`);
    return skill;
  }
}

export function compileProgram(input: unknown): OrchestrationProgram {
  const program = orchestrationProgramSchema.parse(input); const ids = new Set(program.steps.map((step) => step.stepId));
  if (ids.size !== program.steps.length) throw new Error("Program step IDs must be unique");
  if (!ids.has(program.startStepId)) throw new Error("Program startStepId does not exist");
  validateState(program.state.schema, program.state.initial);
  for (const step of program.steps) { for (const target of [step.nextStepId, step.failureStepId]) if (target && !ids.has(target)) throw new Error(`Program step ${step.stepId} targets missing step ${target}`); runtimeInstructionSchema.parse(step.instruction); if (step.outputStateKey && !(step.outputStateKey in program.state.schema)) throw new Error(`Program step ${step.stepId} writes undeclared state key ${step.outputStateKey}`); if (step.background && step.outputStateKey) throw new Error(`Background step ${step.stepId} cannot bind output before an await boundary`); }
  return program;
}

export function validateState(schema: Record<string, "string" | "number" | "boolean" | "ref" | "json">, values: Record<string, JsonValue>): void { if (Buffer.byteLength(JSON.stringify(values)) > 1_000_000) throw new Error("Program state exceeds 1000000 bytes"); for (const key of Object.keys(values)) if (!(key in schema)) throw new Error(`State key ${key} is not declared`); for (const [key, type] of Object.entries(schema)) { const value = values[key]; if (value === undefined) throw new Error(`State key ${key} is required`); const valid = type === "json" ? true : type === "ref" ? typeof value === "string" && /^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$/.test(value) : typeof value === type; if (!valid) throw new Error(`State key ${key} must be ${type}`); } }
