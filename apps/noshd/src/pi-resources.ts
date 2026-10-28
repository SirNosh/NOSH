import { DefaultResourceLoader, SettingsManager, type Skill } from "@earendil-works/pi-coding-agent";
import { resolve } from "node:path";

export async function loadNoshPiResources(workspaceRoot: string): Promise<Skill[]> {
  const loader = new DefaultResourceLoader({
    cwd: workspaceRoot,
    agentDir: resolve(workspaceRoot, ".pi-spike"),
    settingsManager: SettingsManager.inMemory({ packages: [resolve(workspaceRoot, "pi-package")] }),
    noExtensions: true,
    noPromptTemplates: true,
    noThemes: true,
    noContextFiles: true,
  });
  await loader.reload();
  return loader.getSkills().skills;
}
