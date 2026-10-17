/**
 * Imperative NOSH dialogs for the published @opentui/core 0.5.11 API.
 * Interaction reference: OpenCode (MIT), commit
 * b3f1a96c6dd7adeb28b36dd11add1998fc84d67b, packages/tui/src/{ui/dialog.tsx,
 * ui/dialog-select.tsx,component/command-palette.tsx,component/dialog-model.tsx,
 * component/prompt/autocomplete.tsx}. https://github.com/anomalyco/opencode
 * This is an independent implementation, not copied Solid/React source.
 */
import { BoxRenderable, TextRenderable, InputRenderable, InputRenderableEvents, RGBA, type CliRenderer, type KeyEvent, type Renderable } from '@opentui/core';
import type { Model, Project } from './controller.js';
import { COMMANDS, filterPalette, moveSelection, dialogText, projectFormCommand, validateProjectForm, actionFingerprint, type PaletteItem, type CommandItem, type ProjectFormValues } from './palette.js';

export interface DialogTheme { bg:string; panel:string; text:string; muted:string; accent:string; border:string; selected:string; error:string }
const defaultTheme: DialogTheme = {bg:'#141414',panel:'#1e1e1e',text:'#eeeeee',muted:'#9a9a9a',accent:'#5fd4c4',border:'#3c3c3c',selected:'#262626',error:'#e06c75'};
export interface DialogPendingAction {
  path: string;
  body: Record<string, unknown>;
  projectId: string;
  title: string;
  fields: {label:string;value:string}[];
}
/** The dialogs never own host requests, auth, versions, or idempotency keys. */
export interface DialogController {
  projects: Project[];
  models: Model[];
  projectId: string;
  busy: boolean;
  selection: {model:{provider:string;id:string};thinkingLevel:string} | undefined;
  execute(command:string): Promise<void>;
  loadProjects(): Promise<Project[]>;
  loadModels(): Promise<Model[]>;
  selectProject(projectId:string): void;
  pendingAction(): DialogPendingAction | undefined;
  /** Model the active agent is running, as provider/id; used when nothing is selected explicitly. */
  activity?(): { model: string };
}
export interface DialogOptions {
  onClose?: () => void;
  onChange?: () => void;
  submit?: (command:string) => void | Promise<void>;
  setComposer?: (value:string) => void;
  quit?: () => void;
  theme?: Partial<DialogTheme>;
}
interface Choice extends PaletteItem { run: () => void | Promise<void> }
interface SelectState {
  kind:'select'; title:string; choices:Choice[]; filtered:Choice[]; index:number;
  search:InputRenderable; list:BoxRenderable; detail:TextRenderable; footer:TextRenderable;
  hint:string; empty:string; query:string; searchable:boolean;
}
interface FormState {
  kind:'form'; path:InputRenderable; title:InputRenderable; mode:TextRenderable;
  submit:TextRenderable; cancel:TextRenderable; error:TextRenderable;
  values:ProjectFormValues; focus:number; submitted:boolean;
}
type DialogState = SelectState | FormState;

/** Route handleKey before workspace shortcuts. Printable keys still reach the modal input. */
export class DialogManager {
  private overlay: BoxRenderable | undefined;
  private panel: BoxRenderable | undefined;
  private state: DialogState | undefined;
  private previousFocus: Renderable | null = null;
  private generation = 0;
  private locked = false;
  private readonly theme: DialogTheme;
  private readonly onResize = () => { this.layout(); if (this.state?.kind === 'select') this.paintChoices(); };
  constructor(private readonly renderer: CliRenderer, private readonly controller: DialogController, private readonly options: DialogOptions = {}) {
    this.theme = {...defaultTheme,...options.theme};
    renderer.on('resize', this.onResize);
  }
  get isOpen(): boolean { return !!this.overlay; }

  private begin(title:string): BoxRenderable {
    if (!this.overlay) {
      this.previousFocus = this.renderer.currentFocusedRenderable;
      this.previousFocus?.blur();
    }
    this.removeOverlay();
    this.generation++;
    this.locked = false;
    const overlay = new BoxRenderable(this.renderer, {
      id:'nosh-dialog-overlay',position:'absolute',left:0,top:0,width:'100%',height:'100%',
      zIndex:3000,alignItems:'center',backgroundColor:RGBA.fromInts(0,0,0,160),
      onMouseDown:event => { event.stopPropagation(); },
      onMouseUp:event => { event.stopPropagation(); },
    });
    const panel = new BoxRenderable(this.renderer, {
      id:'nosh-dialog-panel',width:76,maxWidth:'96%',flexDirection:'column',padding:1,
      backgroundColor:this.theme.panel,border:true,borderColor:this.theme.border,
    });
    panel.add(new TextRenderable(this.renderer,{id:'nosh-dialog-title',height:1,content:dialogText(title),fg:this.theme.accent}));
    this.overlay = overlay; this.panel = panel;
    overlay.add(panel); this.renderer.root.add(overlay); this.layout();
    this.options.onChange?.();
    return panel;
  }
  private layout(): void {
    if (!this.overlay || !this.panel) return;
    this.overlay.paddingTop = Math.max(0, Math.min(4, Math.floor((this.renderer.height - 12) / 3)));
    this.panel.maxHeight = Math.max(4, this.renderer.height - 1);
    this.panel.maxWidth = Math.max(8, this.renderer.width - 2);
  }
  private removeOverlay(): void {
    this.state = undefined;
    if (this.overlay) {
      this.renderer.root.remove(this.overlay);
      this.overlay.destroyRecursively();
    }
    this.overlay = undefined; this.panel = undefined;
  }
  close(): void {
    if (!this.isOpen) return;
    this.generation++; this.locked = false;
    this.removeOverlay();
    const previousFocus = this.previousFocus; this.previousFocus = null;
    if (previousFocus && !previousFocus.isDestroyed) previousFocus.focus();
    this.options.onClose?.(); this.options.onChange?.();
  }
  destroy(): void { this.renderer.off('resize', this.onResize); this.close(); }

  private select(title:string, choices:Choice[], settings:{query?:string;hint?:string;empty?:string;current?:string;searchable?:boolean} = {}): void {
    const panel = this.begin(title);
    const searchable = settings.searchable !== false;
    const search = new InputRenderable(this.renderer,{
      id:'nosh-dialog-search',width:'100%',placeholder:searchable ? 'Type to search…' : 'Use ↑ / ↓ to choose',
      value:settings.query ?? '',maxLength:200,textColor:this.theme.text,
      backgroundColor:this.theme.panel,focusedBackgroundColor:this.theme.panel,cursorColor:this.theme.accent,
    });
    const list = new BoxRenderable(this.renderer,{id:'nosh-dialog-list',width:'100%',flexDirection:'column',marginTop:1});
    const detail = new TextRenderable(this.renderer,{id:'nosh-dialog-detail',width:'100%',height:2,fg:this.theme.muted,content:''});
    const footer = new TextRenderable(this.renderer,{id:'nosh-dialog-footer',width:'100%',height:1,fg:this.theme.muted,content:''});
    const query = settings.query ?? '';
    const filtered = filterPalette(choices,query);
    const current = filtered.findIndex(choice => choice.id === settings.current);
    this.state = {kind:'select',title,choices,filtered,index:filtered.length ? Math.max(0,current) : -1,search,list,detail,footer,query,searchable,
      hint:settings.hint ?? '↑↓ select   Enter choose   Esc close',empty:settings.empty ?? 'No matches. Try another search.'};
    panel.add(search); panel.add(list); panel.add(detail); panel.add(footer);
    search.on(InputRenderableEvents.INPUT, () => {
      const state = this.state;
      if (state?.kind !== 'select' || state.search !== search || !state.searchable || this.locked) return;
      state.query = search.value; state.filtered = filterPalette(state.choices,search.value);
      state.index = state.filtered.length ? 0 : -1;
      this.paintChoices();
    });
    this.paintChoices(); search.focus();
  }
  private paintChoices(): void {
    const state = this.state;
    if (state?.kind !== 'select') return;
    for (const child of state.list.getChildren()) { state.list.remove(child); child.destroyRecursively(); }
    const visible = Math.max(1, Math.min(9, this.renderer.height - 12));
    const start = Math.max(0, Math.min(state.index - Math.floor(visible / 2), state.filtered.length - visible));
    const rows = state.filtered.slice(start,start + visible);
    const width = Math.max(10,Math.min(70,this.renderer.width - 8));
    if (!rows.length) state.list.add(new TextRenderable(this.renderer,{id:'nosh-dialog-empty',height:2,fg:this.theme.muted,content:dialogText(state.empty,200)}));
    rows.forEach((choice, offset) => {
      const index = start + offset, selected = index === state.index;
      // Shortcuts are right-aligned like OpenCode; category is the fallback.
      const label = `${selected ? '›' : ' '} ${choice.title}`, tag = choice.keybind ?? choice.category ?? '';
      const gap = Math.max(2, width - label.length - tag.length);
      state.list.add(new TextRenderable(this.renderer,{
        id:`nosh-dialog-choice-${index}`,height:1,width:'100%',
        content:dialogText(tag ? label + ' '.repeat(gap) + tag : label,width),
        fg:selected ? this.theme.accent : this.theme.text,bg:selected ? this.theme.selected : this.theme.panel,
        onMouseDown:event => { event.stopPropagation(); state.index = index; this.paintChoices(); },
        onMouseUp:event => { event.stopPropagation(); if (!this.locked && this.state === state) this.choose(); },
      }));
    });
    state.detail.content = dialogText(state.filtered[state.index]?.description ?? '',Math.max(40,width * 2));
    state.footer.content = dialogText(`${this.locked ? 'Working…  Esc dismiss' : state.hint}  ${state.filtered.length ? `${state.index+1}/${state.filtered.length}` : ''}`,width);
    this.renderer.requestRender();
  }
  private choose(): void {
    const state = this.state;
    if (this.locked || state?.kind !== 'select') return;
    const choice = state.filtered[state.index];
    if (!choice || choice.disabled) return;
    this.run(choice.run);
  }
  private run(action:() => void | Promise<void>): void {
    if (this.locked) return;
    const generation = this.generation;
    this.locked = true; this.paintChoices();
    let result: void | Promise<void>;
    try { result = action(); }
    catch (error) { this.failed(error,generation); return; }
    void Promise.resolve(result).then(() => {
      if (this.generation === generation) this.close();
      this.options.onChange?.();
    }, error => this.failed(error,generation));
  }
  private failed(error:unknown,generation:number): void {
    if (this.generation !== generation) { this.options.onChange?.(); return; }
    this.select('Action did not complete',[
      {id:'close',title:'Close',category:'',description:'No automatic retry. Inspect current state before trying again.',run:() => this.close()},
    ],{searchable:false});
    if (this.state?.kind === 'select') { this.state.detail.content = dialogText(error instanceof Error ? error.message : error,220); this.state.detail.fg = this.theme.error; }
    this.options.onChange?.();
  }
  private execute(command:string): Promise<void> | void {
    return this.options.submit ? this.options.submit(command) : this.controller.execute(command);
  }
  openPalette(query = ''): void {
    this.select('Commands', COMMANDS.map(command => ({...command,run:() => this.command(command)})), {query});
  }
  openSlash(query = ''): void {
    // The composer holds only the slash trigger; choosing a command consumes it.
    this.select('Slash commands', COMMANDS.map(command => ({...command,title:command.command.trim() + '  ' + command.title,run:() => { this.options.setComposer?.(''); return this.command(command); }})),{
      query:query.replace(/^\//,''),hint:'↑↓ select   Enter choose   Esc close',
    });
  }
  private command(command:CommandItem): void | Promise<void> {
    if (command.mode === 'quit') { this.close(); this.options.quit?.(); return; }
    if (command.mode === 'compose') { this.close(); this.options.setComposer?.(command.command); return; }
    if (command.mode === 'execute') return this.execute(command.command);
    switch (command.id) {
      case 'projects': this.openProjects(); return;
      case 'models': this.openModels(); return;
      case 'thinking': this.openThinking(); return;
      case 'open': this.openProjectForm(false); return;
      case 'new': this.openProjectForm(true); return;
      case 'confirm': this.openConfirmation(); return;
    }
  }
  private async load<T>(title:string, request:() => Promise<T>, ready:(data:T) => void): Promise<void> {
    this.select(title,[],{empty:'Loading…',searchable:false});
    this.locked = true; this.paintChoices();
    const generation = this.generation;
    try {
      const data = await request();
      if (this.generation !== generation || !this.isOpen) return;
      this.locked = false; ready(data);
    } catch (error) { this.failed(error,generation); }
  }
  openProjects(): void {
    void this.load('Switch project',() => this.controller.loadProjects(), projects => {
      const choices: Choice[] = projects.map(project => ({
        id:project.projectId,title:project.repositoryRoot.split(/[\\/]/).filter(Boolean).at(-1) ?? project.projectId,
        category:project.projectId === this.controller.projectId ? 'Current project' : 'Project',
        description:project.repositoryRoot,keywords:project.projectId,
        run:() => { this.controller.selectProject(project.projectId); this.options.onChange?.(); },
      }));
      choices.push({id:'__open',title:'Open an existing project…',category:'Project',description:'Enter a repository path and working title',run:() => this.openProjectForm(false)});
      choices.push({id:'__new',title:'Create a new project…',category:'Project',description:'Create a repository and start project intake',run:() => this.openProjectForm(true)});
      this.select('Switch project',choices,{current:this.controller.projectId});
    });
  }
  openModels(): void {
    void this.load('Select model',() => this.controller.loadModels(), models => {
      const choices:Choice[] = [
        {id:'__default',title:'Use daemon default',category:'Model',description:'Clear the explicit model and thinking selection',run:() => this.execute('/model default')},
        ...models.map(model => ({
          id:model.provider + '/' + model.id,title:model.name || model.id,category:model.provider,
          description:`${model.provider}/${model.id} · thinking: ${model.thinkingLevels.join(', ') || 'none advertised'}`,
          keywords:model.id,disabled:!model.thinkingLevels.length,
          run:() => this.pickThinking(model),
        })),
      ];
      const selected = this.controller.selection?.model, running = this.controller.activity?.().model ?? '';
      const current = selected ? selected.provider + '/' + selected.id : choices.some(choice => choice.id === running) ? running : '__default';
      this.select('Select model',choices,{current,
        hint:models.length ? '↑↓ select   Enter thinking levels   Esc close' : 'No authenticated models · Esc close'});
    });
  }
  openThinking(): void {
    const selected = this.controller.selection;
    // Thinking levels belong to a model; choose one first.
    if (!selected) { this.openModels(); return; }
    void this.load('Thinking level',() => this.controller.loadModels(), models => {
      const model = models.find(model => model.provider === selected.model.provider && model.id === selected.model.id);
      if (!model) throw new Error('Selected model is no longer available. Choose a model again.');
      this.pickThinking(model);
    });
  }
  private pickThinking(model:Model): void {
    // The provider catalog is authoritative. Do not invent "off" or any other level.
    this.select(`Thinking · ${model.name || model.id}`,model.thinkingLevels.map(level => ({
      id:level,title:level,category:model.provider,description:`${model.provider}/${model.id} · ${level}`,
      run:() => this.execute(`/model ${model.provider} ${model.id} ${level}`),
    })),{...(this.controller.selection ? {current:this.controller.selection.thinkingLevel} : {}),empty:'This model advertises no supported thinking levels. Esc to close.'});
  }
  openProjectForm(createRepository = false): void {
    const panel = this.begin(createRepository ? 'New project' : 'Open project');
    panel.add(new TextRenderable(this.renderer,{height:2,fg:this.theme.muted,content:'Enter a local repository path and working title.\nOpening starts project intake; no contract is approved.'}));
    panel.add(new TextRenderable(this.renderer,{height:1,fg:this.theme.muted,content:'Repository path'}));
    const path = this.formInput('nosh-dialog-project-path','C:\\research\\project or /home/me/project',4096);
    panel.add(path);
    panel.add(new TextRenderable(this.renderer,{height:1,fg:this.theme.muted,content:'Working title'}));
    const title = this.formInput('nosh-dialog-project-title','What are we investigating?',240); panel.add(title);
    const mode = new TextRenderable(this.renderer,{id:'nosh-dialog-project-mode',height:1,content:'',fg:this.theme.text});
    const submit = new TextRenderable(this.renderer,{id:'nosh-dialog-project-submit',height:1,content:'',fg:this.theme.accent});
    const cancel = new TextRenderable(this.renderer,{id:'nosh-dialog-project-cancel',height:1,content:'Cancel',fg:this.theme.muted});
    const error = new TextRenderable(this.renderer,{id:'nosh-dialog-project-error',height:2,content:'',fg:this.theme.error});
    panel.add(mode); panel.add(submit); panel.add(cancel); panel.add(error);
    panel.add(new TextRenderable(this.renderer,{height:1,content:'Tab / ↑↓ focus   Enter select   Esc cancel',fg:this.theme.muted}));
    this.state = {kind:'form',path,title,mode,submit,cancel,error,values:{path:'',workingTitle:'',createRepository},focus:0,submitted:false};
    const state = this.state;
    path.on(InputRenderableEvents.INPUT,() => { state.values.path = path.value; state.error.content = ''; });
    title.on(InputRenderableEvents.INPUT,() => { state.values.workingTitle = title.value; state.error.content = ''; });
    this.focusForm(0);
  }
  private formInput(id:string,placeholder:string,maxLength:number): InputRenderable {
    return new InputRenderable(this.renderer,{id,placeholder,maxLength,width:'100%',textColor:this.theme.text,
      backgroundColor:this.theme.selected,focusedBackgroundColor:this.theme.selected,cursorColor:this.theme.accent});
  }
  private focusForm(index:number): void {
    const state = this.state;
    if (state?.kind !== 'form') return;
    state.focus = moveSelection(index,0,5);
    state.path.blur(); state.title.blur();
    if (state.focus === 0) state.path.focus();
    else if (state.focus === 1) state.title.focus();
    state.mode.content = `${state.focus === 2 ? '›' : ' '} [${state.values.createRepository ? 'x' : ' '}] Create a new repository`;
    state.mode.fg = state.focus === 2 ? this.theme.accent : this.theme.text;
    state.submit.content = `${state.focus === 3 ? '›' : ' '} ${state.values.createRepository ? 'Create project' : 'Open project'} and start intake`;
    state.submit.bg = state.focus === 3 ? this.theme.selected : this.theme.panel;
    state.cancel.content = `${state.focus === 4 ? '›' : ' '} Cancel`;
    state.cancel.fg = state.focus === 4 ? this.theme.accent : this.theme.muted;
    this.renderer.requestRender();
  }
  private submitForm(): void {
    const state = this.state;
    if (state?.kind !== 'form' || this.locked || state.submitted) return;
    const values = {...state.values,path:state.path.value,workingTitle:state.title.value};
    const error = validateProjectForm(values);
    if (error) { state.error.content = error; return; }
    if (this.controller.busy) { state.error.content = 'Wait for the current command.'; return; }
    state.submitted = true;
    state.error.fg = this.theme.muted; state.error.content = 'Starting intake… Do not retry after an uncertain result.';
    this.run(() => this.execute(projectFormCommand(values)));
  }
  openConfirmation(): void {
    const pending = this.controller.pendingAction();
    if (!pending) {
      this.select('Review staged action',[],{empty:'No staged action. Inspect state and stage an action first.',searchable:false}); return;
    }
    const fingerprint = actionFingerprint(pending);
    const check = () => {
      const current = this.controller.pendingAction();
      if (!current || current.projectId !== this.controller.projectId || actionFingerprint(current) !== fingerprint)
        throw new Error('The staged action changed. Close this dialog and inspect the current action.');
    };
    this.select(`Confirm · ${pending.title.replace(/^Review /, '')}`, [
      {id:'keep',title:'Go back — keep staged',category:'Safe default',description:'Close this review. The action stays staged; nothing is applied.',run:() => this.close()},
      {id:'discard',title:'Discard staged action',category:'No mutation',description:'Remove this staged action without applying it.',run:() => { check(); return this.execute('/discard'); }},
      {id:'apply',title:'Apply this action once',category:'Explicit confirmation',description:'Send the request above exactly once. It is not retried after a network failure.',run:() => { check(); return this.execute('/confirm'); }},
    ],{searchable:false,hint:'↑↓ review   Enter choose   Esc keep staged'});
    // Show each inspected field, rather than making the user decode request JSON.
    const state = this.state;
    if (state?.kind === 'select' && this.panel) {
      const summary = [pending.path,...pending.fields.map(field => `${field.label}: ${field.value}`)];
      const limit = Math.max(1,Math.min(8,this.renderer.height - 17));
      const lines = summary.slice(0,limit).map(line => dialogText(line,Math.max(10,Math.min(70,this.renderer.width - 8))));
      if (summary.length > limit) lines.push('More fields in the staged-action inspector. Esc to review.');
      this.panel.insertBefore(new TextRenderable(this.renderer,{id:'nosh-dialog-action-summary',height:lines.length,fg:this.theme.text,content:lines.join('\n')}),state.list);
    }
  }
  handleKey(key:KeyEvent): boolean {
    if (!this.isOpen) return false;
    // Ctrl+C remains the workspace's global detach key.
    if (key.ctrl && key.name === 'c') return false;
    const consume = () => { key.preventDefault(); key.stopPropagation(); };
    if (key.name === 'escape') { consume(); this.close(); return true; }
    if (this.locked) { consume(); return true; }
    const state = this.state;
    if (state?.kind === 'select') {
      if (['up','down','pageup','pagedown','tab'].includes(key.name)) {
        consume();
        const delta = key.name === 'up' || (key.name === 'tab' && key.shift) ? -1 : key.name === 'pageup' ? -8 : key.name === 'pagedown' ? 8 : 1;
        state.index = moveSelection(state.index,delta,state.filtered.length); this.paintChoices();
      } else if (key.name === 'return' || key.name === 'enter') { consume(); this.choose(); }
      else if (!state.searchable || (key.ctrl && ['p','o','t'].includes(key.name)) || key.name === 'f2') consume();
      return true;
    }
    if (state?.kind === 'form') {
      if (['tab','up','down'].includes(key.name)) {
        consume(); this.focusForm(moveSelection(state.focus,key.name === 'up' || key.shift ? -1 : 1,5));
      } else if (key.name === 'return' || key.name === 'enter') {
        consume();
        if (state.focus < 2) this.focusForm(state.focus + 1);
        else if (state.focus === 2) { state.values.createRepository = !state.values.createRepository; this.focusForm(2); }
        else if (state.focus === 3) this.submitForm();
        else this.close();
      } else if (key.name === 'space' && state.focus === 2) {
        consume(); state.values.createRepository = !state.values.createRepository; this.focusForm(2);
      } else if (state.focus >= 2 || (key.ctrl && ['p','o','t'].includes(key.name)) || key.name === 'f2') consume();
      return true;
    }
    consume(); return true;
  }
}
export function createDialogs(renderer:CliRenderer, controller:DialogController, options:DialogOptions = {}): DialogManager {
  return new DialogManager(renderer,controller,options);
}
