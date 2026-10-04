import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { DaemonClient } from './client.js';
import { safeText, record, text, fields, formatFields, modelName, entityRow, responseRows, boundRows, rawDetail, eventEntries, boundEntries, collapseLines, type ViewRow, type TranscriptEntry, type ActivityState, type PendingAction } from './view-model.js';
// Cold Pi session start can exceed the default request timeout; a timeout would
// report failure for a chat the daemon still accepts.
const CHAT_TIMEOUT = 120_000;
export { safeText } from './view-model.js';
export type { ViewField, ViewRow, TranscriptEntry, ActivityState, PendingAction } from './view-model.js';
export interface Project { projectId: string; repositoryRoot: string }
export interface Model { provider: string; id: string; name: string; thinkingLevels: string[] }
export interface NoshEvent { eventId: string; sequence: number | null; type: string; payload: Record<string, unknown>; scope: { projectId: string; agentId?: string | null; jobId?: string | null; missionId?: string | null; directionId?: string | null; autoresearchId?: string | null }; correlationId?: string | null; timestamp?: string }
export interface Stored { entityId: string; version: number; state: string; value: Record<string, unknown> }
export const HELP = `NOSH / research workspace
Type a message to chat with the project's Nosh agent.

Keys
  enter                 send
  shift+enter           newline (also alt+enter)
  ctrl+p                commands
  ctrl+o                switch project
  f2                    select model
  ctrl+t                next thinking level
  ctrl+b                toggle research sidebar
  esc                   back to conversation
  esc esc               clear the draft
  up / down             input history
  pgup / pgdn           scroll
  ctrl+home / ctrl+end  top / bottom
  ctrl+c                detach (daemon and jobs keep running)

Project and model
  /projects                       list registered projects
  /project <id>                   select project
  /open | /new                    open or create a repository (form)
  /open <JSON>                    {"path":"/repo","workingTitle":"Study","createRepository":false}
  /models                         list authenticated models
  /model <provider> <id> [thinking] | /model default
  /thinking                       choose a thinking level for the selected model

Research state
  /chat                           return to conversation
  /status [missions|directions|autoresearch|agents]
  /jobs                           supervised jobs
  /job <id>                       details + resources
  /tail <id> [stdout|stderr]      bounded job output
  /approvals                      graph proposals and project contract
  /paths                          repository, paper and contract paths
  /refresh                        reload current view

Staged actions (nothing is applied until /confirm)
  /cancel <job-id>                /checkpoint <job-id>
  /approve <proposalId> <version>
  /transition <missions|directions|autoresearch> <id> <version> <state>
  /retry <missions|directions> <id> <version> <nodeId>   return a blocked/failed node to ready
  /control <missionId> <version> <pause|resume|stop> [safe|checkpoint|immediate]
  /steer <missionId> <version> <message>   bounded Mission Director steer
  /create <missions|directions|autoresearch> <JSON file path>
  /amend-contract <JSON file path>   next approved Project contract version (e.g. runnable commands)
  /confirm                        apply staged action once
  /discard                        discard staged action

Ids and versions for staged actions are shown on each card in /status, /jobs and /approvals.
Mutations are never retried after network failures. Inspect state before retrying.
/quit detaches; the daemon and supervised jobs keep running.`;
export function acceptPage(current: NoshEvent[], page: NoshEvent[], projectId: string, after: number): { events: NoshEvent[]; after: number } {
  const ids = new Set(current.map(e => e.eventId));
  const accepted = page.filter(e => e.scope?.projectId === projectId && Number.isSafeInteger(e.sequence) && e.sequence! > after)
    .sort((a,b) => a.sequence! - b.sequence!).filter(e => { if (ids.has(e.eventId)) return false; ids.add(e.eventId); return true; });
  return { events: [...current, ...accepted].slice(-400), after: Math.max(after, ...accepted.map(e => e.sequence!)) };
}
export class Controller {
  projects: Project[] = [];
  models: Model[] = [];
  projectId = '';
  events: NoshEvent[] = [];
  cursor = 0;
  view = 'chat';
  detail = HELP;
  connection = 'connecting';
  error = '';
  busy = false;
  selection: { model: {provider: string; id: string}; thinkingLevel: string } | undefined;
  private generation = 0;
  private pollingGeneration: number | undefined;
  private bootstrapped = false;
  private pending: { path: string; body: Record<string, unknown> } | undefined;
  private rows: ViewRow[] = [];
  private readonly sections = new Map<string, ViewRow[]>();
  private readonly sectionCursors = new Map<string, number>();
  private readonly liveIds = new Set<string>();
  private readonly listeners = new Set<() => void>();
  private readonly drafts = new Map<string, TranscriptEntry>();
  private liveEnabled = false;
  private streamProject = '';
  private streamClose: (() => void) | undefined;
  private catalogGeneration = { projects: 0, models: 0 };
  private agentMetadata: Record<string, unknown>[] = [];
  private agentsRequestedAt = 0;
  constructor(readonly client: DaemonClient, projectId = '') { this.projectId = projectId; }
  async initialize(): Promise<void> {
    await this.client.authenticate();
    await this.loadProjects();
    if (this.projectId && !this.projects.some(p => p.projectId === this.projectId)) throw new Error('Selected project is not registered');
    this.projectId ||= this.projects[0]?.projectId ?? '';
    this.connection = 'connected'; this.liveEnabled = true;
  }
  async poll(): Promise<void> {
    // A switched project polls immediately; the stale request is discarded by generation.
    if (this.pollingGeneration === this.generation || !this.projectId) return;
    const project = this.projectId, generation = this.generation;
    this.pollingGeneration = generation;
    try {
      const page = await this.client.request<{events:NoshEvent[]}>(`/events?projectId=${encodeURIComponent(project)}&after=${this.cursor}&limit=200${!this.bootstrapped && this.cursor === 0 ? "&recent=true" : ""}`);
      if (generation !== this.generation) return;
      const next = acceptPage(this.events, page.events, project, this.cursor);
      const priorCursor = this.cursor;
      this.events = next.events; this.cursor = next.after; this.bootstrapped = true; this.connection = 'connected';
      this.reconcileDrafts(page.events.filter(e => e.sequence !== null && e.sequence > priorCursor));
      this.ensureStream(); this.notify();
      void this.refreshAgentMetadata(generation);
    } catch (error) { if (generation === this.generation) this.connection = `reconnecting: ${safeText(error instanceof Error ? error.message : error)}`; }
    finally { if (this.pollingGeneration === generation) this.pollingGeneration = undefined; }
  }
  /** Agent events carry no role/model; load them once per unknown agent (throttled). */
  private async refreshAgentMetadata(generation: number): Promise<void> {
    const known = new Set(this.agentMetadata.map(a => text(a.agentId)));
    const unknown = this.events.some(e => { const id = text(e.scope.agentId); return id && e.type.startsWith('agent.') && !known.has(id); });
    if (!unknown || Date.now() - this.agentsRequestedAt < 5000) return;
    this.agentsRequestedAt = Date.now();
    try {
      const data = await this.client.request<{agents?: unknown[]}>('/agents'+this.query());
      if (generation !== this.generation) return;
      this.agentMetadata = (Array.isArray(data.agents) ? data.agents : []).slice(0,80).map(record); this.notify();
    } catch { /* Metadata is presentation only; the next poll retries. */ }
  }
  /** Catalog reads do not navigate or stage an action. Latest request wins. */
  async loadProjects(): Promise<Project[]> {
    const token = ++this.catalogGeneration.projects;
    const result = await this.client.request<{projects: Project[]}>('/projects');
    if (token === this.catalogGeneration.projects) this.projects = result.projects.slice(0,1000);
    return this.projects.map(p => ({...p}));
  }
  async loadModels(): Promise<Model[]> {
    const token = ++this.catalogGeneration.models;
    const result = await this.client.request<{models: Model[]}>('/models');
    if (token === this.catalogGeneration.models) this.models = result.models.slice(0,1000);
    return this.models.map(m => ({...m,thinkingLevels:[...m.thinkingLevels]}));
  }
  selectProject(projectId: string, preserveModel = false): void {
    const project = this.projects.find(p => p.projectId === projectId);
    if (!project) throw new Error('Unknown project. Run /projects first.');
    this.generation++; this.streamClose?.(); this.streamClose = undefined; this.streamProject = '';
    this.projectId = project.projectId; this.events = []; this.cursor = 0; this.bootstrapped = false;
    this.pending = undefined; if (!preserveModel) this.selection = undefined;
    this.rows = []; this.sections.clear(); this.sectionCursors.clear(); this.agentMetadata = []; this.agentsRequestedAt = 0; this.drafts.clear(); this.liveIds.clear();
    this.view = 'chat'; this.detail = ''; this.error = ''; this.notify();
  }
  pendingAction(): PendingAction | undefined {
    if (!this.pending) return undefined;
    const body = structuredClone(this.pending.body);
    return {path:this.pending.path,body,projectId:String(body.projectId),title:`Review ${this.pending.path.split('/').filter(Boolean).at(-1) ?? 'action'}`,fields:fields(body)};
  }
  inspectorRows(): ViewRow[] { return boundRows(this.rows); }
  sidebarRows(): ViewRow[] {
    const rows = new Map<string,ViewRow>();
    for (const entries of this.sections.values()) for (const row of entries) rows.set(`${row.section}:${row.id}`,row);
    // Durable job and proposal updates fill the sidebar without fetching every panel.
    for (const e of this.events) {
      const p = record(e.payload);
      if (e.scope.projectId !== this.projectId) continue;
      const family = e.type.split('.')[0], section = family === 'mission' ? 'missions' : family === 'direction' ? 'directions' : family === 'autoresearch' ? 'autoresearch' : '';
      if (section && p.entityId && p.state && (e.sequence ?? 0) > (this.sectionCursors.get(section) ?? 0)) {
        const id = text(p.entityId), prior = rows.get(`${section}:${id}`);
        rows.set(`${section}:${id}`,{id,section,title:prior?.title ?? id,state:text(p.state),fields:fields(p)});
      }
      if (e.type === 'job.state_changed' && (e.sequence ?? 0) > (this.sectionCursors.get('jobs') ?? 0)) {
        const row = entityRow('jobs',{...p,jobId:p.jobId ?? e.scope.jobId}); rows.set(`jobs:${row.id}`,row);
      } else if (['graph.proposal_pending','graph.proposal_approved'].includes(e.type) && (e.sequence ?? 0) > (this.sectionCursors.get('approvals') ?? 0)) {
        const id = text(p.proposalId); if (!id) continue;
        const prior = rows.get(`approvals:${id}`);
        rows.set(`approvals:${id}`,{id,section:'approvals',title:prior?.title ?? 'Graph proposal',state:e.type.endsWith('_approved') ? 'approved' : 'pending',fields:prior?.fields ?? fields(p)});
      }
    }
    return boundRows([...rows.values()].slice(-40),40);
  }
  activity(): ActivityState {
    const active = new Map<string,string>(), selections = new Map<string,Record<string,unknown>>(); let latestAgent = ''; 
    for (const e of this.events) {
      if (e.scope.projectId !== this.projectId) continue;
      const p = record(e.payload), agent = text(e.scope.agentId ?? p.agentId);
      if (!agent || !e.type.startsWith('agent.') && e.type !== 'chat.user_message') continue;
      latestAgent = agent;
      if (e.type === 'chat.user_message') {
        selections.set(agent,record(p.selection));
      }
      if (['chat.user_message','agent.started','agent.turn_started','agent.retrying','agent.tool_started'].includes(e.type)) active.set(agent,e.type === 'agent.tool_started' ? `Running ${text(p.toolName) || 'tool'}` : e.type === 'agent.retrying' ? 'Retrying provider turn' : 'Working');
      if (e.type === 'agent.completed' || e.type === 'agent.failed' || e.type === 'agent.terminal_receipt' && p.retryAllowed !== true) active.delete(agent);
    }
    for (const draft of this.drafts.values()) if (draft.agentId) active.set(draft.agentId,'Writing');
    const agentId = [...active.keys()].at(-1) ?? latestAgent;
    const inspection = this.agentMetadata.find(a => a.agentId === agentId);
    const explicitModel = this.selection ? modelName(this.selection.model) : '', durableSelection = selections.get(agentId);
    const observedModel = (inspection ? modelName(inspection) : '') || modelName(durableSelection?.model);
    const model = (active.size ? observedModel : explicitModel || observedModel) || 'Default model';
    const thinkingLevel = text(active.size ? inspection?.thinkingLevel ?? durableSelection?.thinkingLevel : this.selection?.thinkingLevel ?? inspection?.thinkingLevel ?? durableSelection?.thinkingLevel);
    const role = text(inspection?.role) || (selections.has(agentId) ? 'nosh' : agentId ? 'agent' : '');
    return {working:active.size > 0,label:active.get(agentId) ?? (this.busy ? 'Sending command' : 'Ready'),model,thinkingLevel,role,...(agentId ? {agentId} : {})};
  }
  transcriptEntries(): TranscriptEntry[] {
    const entries = eventEntries(this.events.filter(e => e.scope.projectId === this.projectId));
    for (const draft of this.drafts.values()) {
      if (draft.kind === 'tool') {
        const tool = entries.find(e => e.id === draft.id); if (tool) { Object.assign(tool,{...draft,title:tool.title}); continue; }
      }
      entries.push(draft);
    }
    return boundEntries(entries);
  }
  subscribe(listener: () => void): () => void { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; }
  private notify(): void { for (const listener of this.listeners) { try { listener(); } catch { /* A renderer must not change command outcomes. */ } } }
  clearLiveDrafts(): void { this.drafts.clear(); this.liveIds.clear(); this.notify(); }
  dispose(): void { this.liveEnabled = false; this.generation++; this.streamClose?.(); this.streamClose = undefined; this.streamProject = ''; this.drafts.clear(); this.listeners.clear(); }
  private ensureStream(): void {
    if (!this.liveEnabled || !this.bootstrapped || this.streamProject === this.projectId) return;
    this.streamClose?.();
    const project = this.projectId, generation = this.generation;
    this.streamProject = project;
    this.streamClose = this.client.subscribe(project,this.cursor,event => {
      if (generation === this.generation && this.projectId === project) this.ingestLiveEvent(event);
    },state => {
      if (generation !== this.generation) return;
      if (state === 'disconnected') this.clearLiveDrafts();
      this.notify();
    });
  }
  ingestLiveEvent(event: NoshEvent): void {
    if (event.scope?.projectId !== this.projectId) return;
    if (event.sequence !== null) {
      const next = acceptPage(this.events,[event],this.projectId,this.cursor);
      if (next.after === this.cursor) return;
      this.events = next.events; this.cursor = next.after; this.reconcileDrafts([event]); this.notify(); return;
    }
    const agentId = text(event.scope.agentId), p = record(event.payload);
    if (!agentId || this.liveIds.has(event.eventId)) return;
    const latest = [...this.events].reverse().find(e => text(e.scope.agentId ?? record(e.payload).agentId) === agentId && ['agent.started','agent.turn_started','agent.completed','agent.failed','agent.retrying','agent.terminal_receipt','chat.user_message'].includes(e.type));
    if (latest && ['agent.completed','agent.failed','agent.terminal_receipt'].includes(latest.type)) return;
    this.liveIds.add(event.eventId); if (this.liveIds.size > 512) this.liveIds.delete(this.liveIds.values().next().value!);
    if (event.type === 'agent.text_delta' && typeof p.delta === 'string') {
      const key = `draft:${this.projectId}:${agentId}`, prior = this.drafts.get(key);
      this.drafts.set(key,{id:key,kind:'assistant',title:'NOSH',text:safeText((prior?.text ?? '') + p.delta),sequence:null,agentId,status:'streaming'});
    } else if (event.type === 'agent.tool_update') {
      if (this.events.some(e => e.type === 'agent.tool_completed' && text(e.scope.agentId) === agentId && record(e.payload).toolCallId === p.toolCallId)) return;
      const key = `tool:${this.projectId}:${agentId}:${text(p.toolCallId) || event.eventId}`;
      const content = record(p.partialResult).content;
      const body = Array.isArray(content) ? content.slice(0,8).filter(v => record(v).type === 'text').map(v => text(record(v).text,2000)).join('\n') : formatFields(fields(p.partialResult,8));
      this.drafts.set(key,{id:key,kind:'tool',title:text(p.toolName) || 'Tool',text:collapseLines(safeText(body)),sequence:null,agentId,status:'running'});
    } else return;
    // Both active agents and output bytes are bounded independently.
    while (this.drafts.size > 16) this.drafts.delete(this.drafts.keys().next().value!);
    this.notify();
  }
  private reconcileDrafts(events: NoshEvent[]): void {
    for (const e of events) {
      if (e.scope.projectId !== this.projectId) continue;
      const agentId = text(e.scope.agentId ?? record(e.payload).agentId);
      if (['agent.completed','agent.failed','agent.retrying','agent.terminal_receipt'].includes(e.type)) {
        for (const [key,draft] of this.drafts) if (draft.agentId === agentId) this.drafts.delete(key);
      }
      if (e.type === 'agent.tool_completed') this.drafts.delete(`tool:${this.projectId}:${agentId}:${text(record(e.payload).toolCallId) || e.eventId}`);
    }
  }
  private present(view: string, rows: ViewRow[], raw: unknown): void {
    this.rows = boundRows(rows); this.view = view; this.detail = rawDetail(raw);
    for (const section of new Set(rows.map(row => row.section))) {
      if (!['missions','directions','autoresearch','agents','jobs','approvals','contract'].includes(section)) continue;
      const incoming = rows.filter(row => row.section === section).slice(0,80);
      const merged = view === 'detail' ? [...(this.sections.get(section) ?? []).filter(row => !incoming.some(v => v.id === row.id)),...incoming] : incoming;
      this.sections.set(section,merged.slice(-80)); this.sectionCursors.set(section,this.cursor);
    }
  }
  private requireProject(): void { if (!this.projectId) throw new Error('Select or open a project first'); }
  private query(): string { this.requireProject(); return `?projectId=${encodeURIComponent(this.projectId)}`; }
  private stage(path: string, body: Record<string, unknown>): void {
    this.pending = { path, body: { ...body, projectId: this.projectId, idempotencyKey: `tui-${randomUUID()}` } };
    this.detail = `REVIEW ACTION\n${safeText(path)}\n${rawDetail(this.pending.body)}\n\n/confirm to apply · /discard to dismiss`;
    this.rows = [{id:'pending',section:'action',title:this.pendingAction()!.title,subtitle:safeText(path),fields:fields(this.pending.body)}];
    this.view = 'action';
  }
  async execute(input: string): Promise<void> {
    if (this.busy) throw new Error('Wait for the current command');
    this.busy = true; this.error = '';
    const generation = this.generation, previous = { view: this.view, detail: this.detail, rows: this.rows };
    try { await this.run(input.trim()); }
    catch (error) { if (generation === this.generation) { this.view = previous.view; this.detail = previous.detail; this.rows = previous.rows; this.error = safeText(error instanceof Error ? error.message : error); } throw error; }
    finally { this.busy = false; this.notify(); }
  }
  private async run(input: string): Promise<void> {
    if (!input) return;
    const generation = this.generation, current = () => generation === this.generation;
    if (!input.startsWith('/')) {
      this.requireProject();
      await this.client.request('/chat', { projectId: this.projectId, message: input, idempotencyKey: `tui-chat-${randomUUID()}`, ...this.selection }, CHAT_TIMEOUT);
      if (current()) this.view = 'chat'; return;
    }
    const [command, ...args] = input.split(/\s+/);
    const rest = input.slice(command!.length).trim();
    switch (command) {
      case '/help': this.view = 'help'; this.detail = HELP; this.rows = []; return;
      case '/chat': this.view = 'chat'; return;
      case '/projects': {
        await this.loadProjects(); if (!current()) return;
        this.present('projects',this.projects.map(p => ({id:p.projectId,section:'projects',title:p.projectId,fields:[{label:'repository',value:safeText(p.repositoryRoot)}]})),{projects:this.projects}); return;
      }
      case '/project': this.selectProject(rest); return;
      case '/open': {
        let body: Record<string, unknown>;
        try { body = JSON.parse(rest) as Record<string, unknown>; } catch { body = {}; }
        if (!body || typeof body.path !== 'string' || typeof body.workingTitle !== 'string' || typeof body.createRepository !== 'boolean') throw new Error('Use /open {"path":"/repo","workingTitle":"Study","createRepository":false}');
        const selection = this.selection ? structuredClone(this.selection) : undefined;
        const {project} = await this.client.request<{project:Project}>('/projects/open', {...body,...selection}, CHAT_TIMEOUT);
        if (!current()) return;
        this.projects = [...this.projects.filter(p => p.projectId !== project.projectId), project];
        this.selectProject(project.projectId,true); this.selection = selection; return;
      }
      case '/models': {
        await this.loadModels(); if (!current()) return;
        this.present('models',this.models.map(m => ({id:`${m.provider}/${m.id}`,section:'models',title:safeText(m.name),subtitle:safeText(`${m.provider}/${m.id}`),fields:[{label:'thinking',value:m.thinkingLevels.map(safeText).join(' · ')}]})),{models:this.models}); return;
      }
      case '/model': {
        if (rest === 'default') { this.selection = undefined; return; }
        if (!this.models.length) await this.loadModels();
        if (!current()) return;
        const model = this.models.find(m => m.provider === args[0] && m.id === args[1]);
        if (!model) throw new Error('Unknown model. Use /models.');
        // Omitted thinking uses "off" only when the provider advertises it.
        const thinkingLevel = args[2] ?? (model.thinkingLevels.includes('off') ? 'off' : model.thinkingLevels[0] ?? '');
        if (!model.thinkingLevels.includes(thinkingLevel)) throw new Error(`Thinking must be one of: ${model.thinkingLevels.join(', ')}`);
        this.selection = {model: {provider: model.provider, id:model.id}, thinkingLevel}; return;
      }
      case '/status': {
        const q = this.query();
        const families = ['missions','directions','autoresearch','agents'];
        if (rest && !families.includes(rest)) throw new Error('Use /status [missions|directions|autoresearch|agents]');
        const sections = await Promise.all((rest ? [rest] : families).map(async family => ({family,data:await this.client.request('/'+family+q)})));
        if (!current()) return;
        const rows = sections.flatMap(({family,data}) => {
          const rows = responseRows(family,data); this.sections.set(family,rows);
          if (family === 'agents') this.agentMetadata = (Array.isArray(record(data).agents) ? record(data).agents as unknown[] : []).slice(0,80).map(record);
          return rows;
        });
        this.present('status',rows,Object.fromEntries(sections.map(s => [s.family,s.data]))); return;
      }
      case '/jobs': {
        const data = await this.client.request('/jobs'+this.query()); if (!current()) return;
        const rows = responseRows('jobs',data); this.sections.set('jobs',rows); this.present('jobs',rows,data); return;
      }
      case '/job': case '/tail': {
        if (!args[0]) throw new Error('Job ID required');
        if (command === '/tail' && args[1] && !['stdout','stderr'].includes(args[1])) throw new Error('Stream must be stdout or stderr');
        const data = await this.client.request(`/jobs/${encodeURIComponent(args[0])}${command === '/tail' ? '/tail' : ''}${this.query()}${command === '/tail' ? '&stream='+(args[1] ?? 'stdout') : ''}`);
        if (!current()) return;
        this.present('detail',responseRows('jobs',data),data); return;
      }
      case '/cancel': case '/checkpoint': this.requireProject(); if (!rest || args.length !== 1) throw new Error('Job ID required'); this.stage(`/jobs/${encodeURIComponent(rest)}/${command.slice(1)}`,{}); return;
      case '/approvals': {
        const q = this.query(), project = this.projectId;
        const [contract,proposals] = await Promise.all([this.client.request(`/projects/${encodeURIComponent(project)}/contract`),this.client.request('/graph-proposals'+q)]);
        if (!current()) return;
        const approvals = responseRows('approvals',proposals); this.sections.set('approvals',approvals);
        this.present('approvals',[...responseRows('contract',contract),...approvals],{contract,proposals}); return;
      }
      case '/approve': this.requireProject(); if (args.length !== 2) throw new Error('Proposal ID and inspected version required'); this.stage(`/graph-proposals/${encodeURIComponent(args[0]!)}/approve`,{expectedProposalVersion: version(args[1])}); return;
      case '/transition': {
        this.requireProject(); if (args.length !== 4 || !['missions','directions','autoresearch'].includes(args[0]!)) throw new Error('Use /transition <family> <id> <inspected-version> <state>');
        this.stage(`/${args[0]}/${encodeURIComponent(args[1]!)}/transition`,{expectedVersion:version(args[2]),next:args[3]}); return;
      }
      case '/retry': {
        this.requireProject(); if (args.length !== 4 || !['missions','directions'].includes(args[0]!)) throw new Error('Use /retry <missions|directions> <id> <inspected-version> <nodeId>');
        this.stage(`/${args[0]}/${encodeURIComponent(args[1]!)}/nodes/${encodeURIComponent(args[3]!)}/transition`,{expectedVersion:version(args[2]),next:'ready'}); return;
      }
      case '/steer': {
        this.requireProject(); const message = rest.split(/\s+/).slice(2).join(' ').trim(); if (args.length < 3 || !message) throw new Error('Use /steer <missionId> <inspected-version> <message>');
        this.stage(`/missions/${encodeURIComponent(args[0]!)}/steer`,{expectedVersion:version(args[1]),message}); return;
      }
      case '/control': {
        this.requireProject(); if (args.length < 3 || args.length > 4 || !['pause','resume','stop'].includes(args[2]!) || !['safe','checkpoint','immediate'].includes(args[3] ?? 'safe')) throw new Error('Use /control <missionId> <version> <pause|resume|stop> [mode]');
        this.stage(`/missions/${encodeURIComponent(args[0]!)}/control`,{expectedVersion:version(args[1]),action:args[2],mode:args[3] ?? 'safe'}); return;
      }
      case '/create': {
        this.requireProject(); const family = args[0];
        if (!family || !['missions','directions','autoresearch'].includes(family)) throw new Error('Use /create <missions|directions|autoresearch> <JSON file path>');
        const text = await readFile(rest.slice(family.length).trim(), 'utf8');
        if (text.length > 256000) throw new Error('Input file exceeds 256 KB');
        const body: unknown = JSON.parse(text); if (!body || typeof body !== 'object' || Array.isArray(body)) throw new Error('Expected a JSON object');
        if (current()) this.stage('/'+family,body as Record<string,unknown>); return;
      }
      case '/amend-contract': {
        this.requireProject(); if (!rest) throw new Error('Use /amend-contract <JSON file path>');
        const text = await readFile(rest, 'utf8'); if (text.length > 256000) throw new Error('Input file exceeds 256 KB');
        const contract: unknown = JSON.parse(text); if (!contract || typeof contract !== 'object' || Array.isArray(contract)) throw new Error('Expected a JSON object');
        if (current()) this.stage(`/projects/${encodeURIComponent(this.projectId!)}/contract/amend`,{contract}); return;
      }
      case '/confirm': {
        if (!this.pending) throw new Error('No staged action');
        const pending = this.pending; this.pending = undefined;
        if (pending.body.projectId !== this.projectId) throw new Error('Staged action belongs to another project');
        const data = await this.client.request(pending.path,pending.body); if (!current()) return;
        this.present('result',responseRows('result',data),data); return;
      }
      case '/discard': this.pending = undefined; this.view = 'chat'; return;
      case '/paths': this.requireProject(); this.view = 'paths'; this.rows = []; this.detail = `${this.projects.find(p => p.projectId === this.projectId)?.repositoryRoot}\n\nOpen this repository in your external editor.\nPaper: docs/paper.md\nContract: .nosh/contracts/project.v<N>.json (active version in .nosh/project.json)\nUse /status and /job for daemon-owned artifact references.\nNo shell is embedded. Contract approval remains an explicit intake conversation.`; return;
      case '/refresh': if (['status','jobs','approvals','projects','models'].includes(this.view)) await this.run('/'+this.view); else await this.poll(); return;
      default: throw new Error('Unknown command. Use /help.');
    }
  }
  transcript(): string {
    return this.transcriptEntries().map(e => `${e.title}  ·  ${e.sequence === null ? 'live' : '#'+e.sequence}${e.status ? ' · '+e.status : ''}\n${e.text}`).join('\n\n') || 'What should we investigate?\n\nOpen a project with /open or choose one with /projects.\nType a message to continue project intake and research.';
  }
}
function version(value: string | undefined): number { const n = Number(value); if (!value || !Number.isSafeInteger(n) || n < 1) throw new Error('A positive inspected version is required'); return n; }
