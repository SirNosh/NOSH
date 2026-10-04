/** Pure search and form rules for the imperative NOSH dialogs. */
export interface PaletteItem {
  id: string;
  title: string;
  category: string;
  description?: string;
  keywords?: string;
  disabled?: boolean;
  /** Workspace shortcut shown beside the title; must match ui.ts handleKey. */
  keybind?: string;
}
export interface CommandItem extends PaletteItem {
  command: string;
  mode: 'dialog' | 'execute' | 'compose' | 'quit';
}
export const COMMANDS: readonly CommandItem[] = [
  {id:'projects',title:'Switch project',category:'Project',description:'Choose a registered repository',keywords:'open directory workspace',command:'/projects',mode:'dialog',keybind:'ctrl+o'},
  {id:'open',title:'Open project',category:'Project',description:'Open an existing repository and begin intake',keywords:'directory folder path',command:'/open',mode:'dialog'},
  {id:'new',title:'New project',category:'Project',description:'Create a repository and begin intake',keywords:'create directory folder',command:'/new',mode:'dialog'},
  {id:'models',title:'Select model',category:'Model',description:'Choose provider and model',keywords:'model provider authenticated',command:'/models',mode:'dialog',keybind:'f2'},
  {id:'thinking',title:'Thinking level',category:'Model',description:'Choose a supported level for the selected model',keywords:'reasoning effort variant',command:'/thinking',mode:'dialog',keybind:'ctrl+t'},
  {id:'chat',title:'Conversation',category:'Research',description:'Return to the current conversation',command:'/chat',mode:'execute',keybind:'esc'},
  {id:'map',title:'Research map',category:'Research',description:'Live tree of missions, directions, autoresearch, workers and jobs',keywords:'graph tree visual overview dashboard progress experiments',command:'/map',mode:'execute',keybind:'ctrl+g'},
  {id:'status',title:'Research status',category:'Research',description:'Missions, directions, autoresearch and agents',command:'/status',mode:'execute'},
  {id:'jobs',title:'Supervised jobs',category:'Research',description:'Inspect jobs and resource use',command:'/jobs',mode:'execute'},
  {id:'approvals',title:'Review approvals',category:'Research',description:'Inspect proposals and the project contract',command:'/approvals',mode:'execute'},
  {id:'paths',title:'Project paths',category:'Project',description:'Repository, paper and artifact paths',command:'/paths',mode:'execute'},
  {id:'refresh',title:'Refresh current view',category:'Workspace',command:'/refresh',mode:'execute'},
  {id:'confirm',title:'Review staged action',category:'Actions',description:'Review before explicitly applying a mutation',keywords:'confirm approve apply',command:'/confirm',mode:'dialog'},
  {id:'discard',title:'Discard staged action',category:'Actions',description:'Dismiss without applying the action',command:'/discard',mode:'execute'},
  {id:'job',title:'Inspect a job',category:'Research',description:'/job <id>',command:'/job ',mode:'compose'},
  {id:'tail',title:'Read job output',category:'Research',description:'/tail <id> [stdout|stderr]',command:'/tail ',mode:'compose'},
  {id:'cancel',title:'Stage job cancellation',category:'Actions',description:'/cancel <job-id> — confirmation required',command:'/cancel ',mode:'compose'},
  {id:'checkpoint',title:'Stage job checkpoint',category:'Actions',description:'/checkpoint <job-id> — confirmation required',command:'/checkpoint ',mode:'compose'},
  {id:'approve',title:'Stage proposal approval',category:'Actions',description:'/approve <proposal-id> <inspected-version>',command:'/approve ',mode:'compose'},
  {id:'transition',title:'Stage state transition',category:'Actions',description:'/transition <family> <id> <inspected-version> <state>',command:'/transition ',mode:'compose'},
  {id:'retry',title:'Stage node retry',category:'Actions',description:'/retry <missions|directions> <id> <inspected-version> <node-id>',command:'/retry ',mode:'compose'},
  {id:'steer',title:'Stage Mission steer',category:'Actions',description:'/steer <mission-id> <inspected-version> <message>',command:'/steer ',mode:'compose'},
  {id:'control',title:'Stage mission control',category:'Actions',description:'/control <id> <inspected-version> <pause|resume|stop>',command:'/control ',mode:'compose'},
  {id:'amend-contract',title:'Stage contract amendment',category:'Actions',description:'/amend-contract <JSON-file-path> — next approved Project contract version',command:'/amend-contract ',mode:'compose'},
  {id:'create',title:'Stage research entity creation',category:'Actions',description:'/create <family> <JSON-file-path>',command:'/create ',mode:'compose'},
  {id:'help',title:'Command reference',category:'Workspace',description:'All commands and safety notes',command:'/help',mode:'execute'},
  {id:'quit',title:'Quit NOSH',category:'Workspace',description:'Detach only; daemon and jobs keep running',command:'/quit',mode:'quit',keybind:'ctrl+c'},
];

/** Strip terminal controls from display text, not from request data. */
export function dialogText(value: unknown, maxLength = 240): string {
  return String(value).replace(/[\x00-\x1f\x7f-\x9f]/g, ' ').slice(0, maxLength);
}
function matchScore(text: string, needle: string): number {
  const value = text.toLowerCase();
  if (value === needle) return 120;
  if (value.startsWith(needle)) return 100;
  const position = value.indexOf(needle);
  if (position >= 0) return 70 - Math.min(position, 30);
  let cursor = 0, first = -1, last = -1;
  for (const character of needle) {
    cursor = value.indexOf(character, cursor);
    if (cursor < 0) return -1;
    if (first < 0) first = cursor;
    last = cursor++;
  }
  return 25 - Math.min(last - first - needle.length + 1, 20);
}
/** Stable, case-insensitive, multi-token fuzzy search. Titles rank above metadata. */
export function filterPalette<T extends PaletteItem>(items: readonly T[], query: string): T[] {
  const tokens = query.trim().replace(/^\//, '').toLowerCase().split(/\s+/).filter(Boolean);
  const ranked = items.filter(item => !item.disabled).map((item, index) => {
    const fields: [string, number][] = [[item.id, 4], [item.title, 3], [item.category, 1], [item.description ?? '', 1], [item.keywords ?? '', 2]];
    const scores = tokens.map(token => Math.max(...fields.map(([text, weight]) => {
      const score = matchScore(text, token);
      return score < 0 ? -1 : score * weight;
    })));
    return {item,index,score:scores.some(score => score < 0) ? -1 : scores.reduce((sum, score) => sum + score, 0)};
  });
  return ranked.filter(row => row.score >= 0).sort((a,b) => b.score - a.score || a.index - b.index).map(row => row.item);
}
export function moveSelection(index: number, delta: number, count: number): number {
  if (count < 1) return -1;
  return ((Math.max(0, index) + delta) % count + count) % count;
}
/** Only a slash command name is autocomplete input; never capture its arguments. */
export function slashQuery(value: string): string | undefined {
  return /^\/[^\s]*$/.test(value) ? value.slice(1) : undefined;
}
export interface ProjectFormValues { path: string; workingTitle: string; createRepository: boolean }
export function validateProjectForm(values: ProjectFormValues): string | undefined {
  if (!values.path.trim()) return 'Enter a repository path.';
  if (!values.workingTitle.trim()) return 'Enter a working title.';
  if (/[\x00-\x1f\x7f-\x9f]/.test(values.path + values.workingTitle)) return 'Path and title must not contain control characters.';
  if (values.path.length > 4096) return 'Repository path is too long (4096 characters maximum).';
  if (values.workingTitle.length > 240) return 'Working title is too long (240 characters maximum).';
  if (typeof values.createRepository !== 'boolean') return 'Choose open existing or create repository.';
  return undefined;
}
/** Paths and titles are data. They are never interpolated into a shell command. */
export function projectFormCommand(values: ProjectFormValues): string {
  const error = validateProjectForm(values);
  if (error) throw new Error(error);
  return '/open ' + JSON.stringify({path:values.path.trim(),workingTitle:values.workingTitle.trim(),createRepository:values.createRepository});
}
export function actionFingerprint(action: {path:string;body:Record<string,unknown>;projectId:string}): string {
  return JSON.stringify([action.projectId, action.path, action.body]);
}
