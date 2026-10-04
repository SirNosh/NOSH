import { stripVTControlCharacters } from 'node:util';
import type { NoshEvent } from './controller.js';

export interface ViewField { label: string; value: string }
export interface ViewRow { id: string; section: string; title: string; state?: string; version?: number; subtitle?: string; fields: ViewField[] }
export interface TranscriptEntry {
  id: string; kind: 'user' | 'assistant' | 'tool' | 'failure' | 'receipt' | 'approval';
  title: string; text: string; sequence: number | null; agentId?: string; model?: string; thinkingLevel?: string; status?: string;
}
export interface ActivityState { working: boolean; label: string; model: string; thinkingLevel: string; agentId?: string; role?: string }
export interface PendingAction { path: string; body: Record<string, unknown>; projectId: string; title: string; fields: ViewField[] }
const MAX_TRANSCRIPT_TEXT = 64000;
export function safeText(value: unknown): string {
  // Drop complete ANSI/OSC sequences, then remaining C0/C1 and bidi controls.
  // Keep LF and TAB, but never CR (it can overwrite a safety label).
  return stripVTControlCharacters(String(value).slice(0, 256000)).replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f\u202a-\u202e\u2066-\u2069]/g, '').slice(0, 16000);
}
export function record(value: unknown): Record<string, unknown> { return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {}; }
export function text(value: unknown, limit = 512): string {
  return ['string','number','boolean'].includes(typeof value) ? safeText(value).slice(0, limit) : '';
}
function label(key: string): string {
  const words = safeText(key.replace(/([a-z])([A-Z])/g, '$1 $2').replace(/[_-]/g, ' ')).toLowerCase().slice(0, 80);
  return words.charAt(0).toUpperCase() + words.slice(1);
}
export function fields(value: unknown, maximum = 16): ViewField[] {
  const result: ViewField[] = [];
  const visit = (data: unknown, prefix: string, depth: number): void => {
    for (const [key, item] of Object.entries(record(data)).slice(0, 40)) {
      if (result.length >= maximum) break;
      // projectId is always the current project; piSessionId is Pi-internal.
      if (['$schema','schemaVersion','idempotencyKey','payload','records','content','thinking','reasoning','projectId','piSessionId'].includes(key)) continue;
      const name = prefix ? `${prefix} / ${label(key)}` : label(key);
      if (item === null || item === undefined) continue;
      if (Array.isArray(item)) {
        const scalars = item.slice(0, 8).map(v => text(v, 120)).filter(Boolean);
        result.push({label:name, value:scalars.length ? scalars.join(' · ') + (item.length > 8 ? ' …' : '') : `${item.length} items`});
      } else if (typeof item === 'object') {
        if (depth < 1) visit(item, name, depth + 1);
      } else result.push({label:name, value:shortDigest(text(item))});
    }
  };
  visit(value, '', 0); return result;
}
/** Full digests are provenance, not reading material: keep a recognizable prefix/suffix. */
function shortDigest(value: string): string {
  const digest = /^(sha256:)?([0-9a-f]{40,64})$/.exec(value);
  return digest ? `${digest[1] ?? ''}${digest[2]!.slice(0, 12)}…${digest[2]!.slice(-4)}` : value;
}
export function formatFields(items: ViewField[]): string { return items.map(f => `${f.label}: ${f.value}`).join('\n'); }
export function modelName(value: unknown): string {
  const m = record(value);
  return [text(m.provider ?? m.modelProvider), text(m.id ?? m.modelId)].filter(Boolean).join('/');
}
export function entityRow(section: string, value: unknown, index = 0): ViewRow {
  const stored = record(value), v = Object.keys(record(stored.value)).length ? record(stored.value) : stored;
  const proposal = record(v.record), resources = record(stored.resources);
  const id = text(stored.entityId ?? v.jobId ?? v.agentId ?? v.proposalId ?? v.id ?? proposal.proposalId) || `${section}-${index}`;
  const title = text(v.title ?? v.question ?? v.decisionQuestion ?? proposal.summary ?? proposal.rationale ?? v.role ?? v.workingTitle) || (section === 'jobs' ? text(Array.isArray(v.command) ? v.command.slice(0, 8).join(' ') : v.command) : '') || id;
  const state = text(stored.state ?? v.state ?? v.status);
  const version = typeof stored.version === 'number' && Number.isSafeInteger(stored.version) ? stored.version : undefined;
  const subtitle = text(v.objective ?? v.decisionUse ?? v.failureReason ?? proposal.contractImpact ?? v.currentTool ?? v.workingDirectory);
  // A job's title is already its command line.
  const {command, ...detail} = v;
  return {id,section,title,...(state ? {state} : {}),...(version === undefined ? {} : {version}),...(subtitle ? {subtitle} : {}),fields:fields({...(section === 'jobs' ? detail : {command,...detail}),...resources})};
}
export function responseRows(section: string, data: unknown): ViewRow[] {
  const body = record(data), listKey: Record<string, string> = {autoresearch:'executions',approvals:'proposals'};
  const list = body[listKey[section] ?? section];
  if (Array.isArray(list)) return list.slice(0, 80).map((v,i) => entityRow(section,v,i));
  if (body.job) return [entityRow('jobs',{...record(body.job), resources:body.resources})];
  if (typeof body.text === 'string') return [{id:'output',section:'output',title:'Bounded job output',fields:[{label:'output',value:safeText(body.text.slice(-16000))}]}];
  if ('contract' in body) {
    if (!body.contract) return [{id:'contract',section:'contract',title:'Project contract',state:'not approved',fields:[{label:'next step',value:'Continue intake in the conversation. Contract approval is explicit.'}]}];
    // The daemon returns the active contract even while it is an unapproved draft.
    const approved = !!record(body.contract).approvedAt;
    return [{...entityRow('contract',body.contract),id:'contract',title:text(record(body.contract).workingTitle) || 'Project contract',
      state:approved ? 'approved' : 'draft',...(approved ? {} : {subtitle:'Not approved. Approval happens explicitly in the intake conversation.'})}];
  }
  const nested = ['mission','direction','execution','proposal'].find(key => body[key]);
  return Object.keys(body).length ? [entityRow(section,nested ? body[nested] : body)] : [];
}
export function boundRows(rows: ViewRow[], maximum = 80): ViewRow[] {
  let left = 48000;
  return rows.slice(0,maximum).map(row => {
    const result: ViewRow = {...row,id:safeText(row.id),section:text(row.section,80),title:text(row.title),fields:[]};
    if (result.subtitle) result.subtitle = text(result.subtitle);
    if (result.state) result.state = text(result.state,80);
    for (const field of row.fields.slice(0,16)) {
      const value = safeText(field.value).slice(0, Math.max(0,left));
      if (!value) break;
      result.fields.push({label:text(field.label,80),value}); left -= value.length;
    }
    return result;
  });
}
/** Bounded raw-command fallback; inspectors use typed rows instead. */
export function rawDetail(value: unknown): string {
  let left = 24000;
  const clip = (v: unknown, depth: number): unknown => {
    if (left <= 0) return '[clipped]';
    if (typeof v === 'string') { const out = safeText(v).slice(0,Math.min(4096,left)); left -= out.length; return out; }
    if (v === null || typeof v !== 'object') return v;
    if (depth > 4) return '[focused detail omitted]';
    if (Array.isArray(v)) return v.slice(0,30).map(item => clip(item,depth+1));
    return Object.fromEntries(Object.entries(v).slice(0,40).map(([k,item]) => [text(k,100),clip(item,depth+1)]));
  };
  return safeText(JSON.stringify(clip(value,0),null,2));
}
function contentText(value: unknown): string {
  const v = record(value);
  if (typeof value === 'string') return safeText(value);
  if (Array.isArray(v.content)) return safeText(v.content.slice(0,20).filter(item => record(item).type === 'text').map(item => text(record(item).text,4000)).join('\n'));
  return text(v.message ?? v.text ?? v.summary ?? v.error,4000) || formatFields(fields(v,8));
}
function assistantText(value: unknown): string {
  const plain = text(value,16000);
  // Terminal envelopes are protocol output, not conversational prose or approval.
  if (typeof value === 'string' && value.length <= 131072 && value.trim().startsWith('{')) {
    try {
      const envelope = record(JSON.parse(value));
      if (envelope.$schema === 'https://nosh.dev/schemas/terminal-output/v1' && Array.isArray(envelope.records)) {
        return envelope.records.slice(0,8).map(item => {
          const r = record(item);
          return text(r.summary ?? r.message ?? r.outcome,2000) || 'Structured result submitted';
        }).join('\n') + '\nAwait the host receipt for validation.';
      }
    } catch { /* Ordinary assistant text. */ }
  }
  return plain;
}
/** One-line target of a tool call (command, path, pattern…) for its transcript title. */
function toolSummary(args: unknown): string {
  const a = record(args);
  const value = text(a.command ?? a.path ?? a.file_path ?? a.pattern ?? a.query ?? a.url, 200).replace(/\s+/g, ' ').trim();
  return value.length > 72 ? value.slice(0, 71) + '…' : value;
}
/** Tool output is evidence, not conversation: show the head, count the rest. */
export function collapseLines(value: string, maximum = 6, characters = 360): string {
  const lines = value.replace(/\n+$/, '').split('\n');
  let head = lines.slice(0, maximum).join('\n');
  // A single long line (minified JSON) also floods the transcript.
  if (head.length > characters) head = head.slice(0, characters - 1) + '…';
  const more = lines.length - maximum;
  return more > 0 ? `${head}\n… ${more} more lines` : head;
}
export function eventEntries(events: NoshEvent[]): TranscriptEntry[] {
  const result: TranscriptEntry[] = [], tools = new Map<string, TranscriptEntry>();
  const selections = new Map<string, {model?: string; thinkingLevel?: string}>();
  for (const e of events.slice(-400)) {
    const p = record(e.payload), agentId = text(e.scope.agentId ?? p.agentId), selection = record(p.selection);
    if (e.type === 'chat.user_message' && agentId) {
      const model = modelName(selection.model), thinkingLevel = text(selection.thinkingLevel);
      selections.set(agentId,{...(model ? {model} : {}),...(thinkingLevel ? {thinkingLevel} : {})});
    }
    const base = {id:e.eventId,sequence:e.sequence,...(agentId ? {agentId,...selections.get(agentId)} : {})};
    if (e.type === 'chat.user_message') result.push({...base,kind:'user',title:'YOU',text:text(p.message,16000)});
    else if (e.type === 'agent.completed') {
      const message = assistantText(p.message ?? p.text);
      if (message) result.push({...base,kind:'assistant',title:'NOSH',text:message,status:'completed'});
    } else if (e.type === 'agent.failed' || e.type === 'agent.retrying') result.push({...base,kind:'failure',title:e.type === 'agent.retrying' ? 'RETRYING' : 'TURN FAILED',text:text(p.message ?? p.error ?? p.reason,4000) || 'The agent turn failed.',status:e.type === 'agent.retrying' ? 'retrying' : 'failed'});
    else if (e.type === 'agent.tool_started' || e.type === 'agent.tool_completed') {
      const toolKey = p.toolCallId ? `${agentId}:${text(p.toolCallId)}` : e.eventId;
      const previous = tools.get(toolKey), complete = e.type === 'agent.tool_completed';
      // Completion events carry no args; keep the started title (tool + target).
      const summary = toolSummary(p.args), title = previous?.title ?? ((text(p.toolName) || 'Tool') + (summary ? ' ' + summary : ''));
      const entry: TranscriptEntry = {...base,id:previous?.id ?? `tool:${e.scope.projectId}:${toolKey}`,kind:'tool',title,text:complete ? collapseLines(contentText(p.result)) : summary ? '' : formatFields(fields(p.args,8)),status:complete ? p.isError ? 'failed' : 'completed' : 'running'};
      if (previous) Object.assign(previous,entry); else { tools.set(toolKey,entry); result.push(entry); }
    } else if (e.type === 'agent.terminal_receipt') {
      const accepted = p.accepted === true, failed = record(p.effect).state === 'failed';
      const status = text(p.status) || (accepted ? 'accepted' : p.retryAllowed === true ? 'correction requested' : 'rejected');
      const detail = [text(p.error ?? p.message,2000),formatFields(fields({status,accepted:p.accepted,retryAllowed:p.retryAllowed,effect:p.effect},6))].filter(Boolean).join('\n');
      result.push({...base,kind:'receipt',title:'HOST RECEIPT',text:detail,status:failed ? 'failed' : status});
    } else if (e.type === 'record.submitted' && p.requiredAuthority === 'user') result.push({...base,kind:'approval',title:'APPROVAL NEEDED',text:[text(p.summary,2000),text(p.requestedAction,2000)].filter(Boolean).join('\n'),status:'pending'});
  }
  return boundEntries(result);
}
export function boundEntries(entries: TranscriptEntry[]): TranscriptEntry[] {
  let left = MAX_TRANSCRIPT_TEXT;
  const result: TranscriptEntry[] = [];
  for (const entry of entries.slice(-120).reverse()) {
    if (left <= 0) break;
    const body = safeText(entry.text).slice(0,left); left -= body.length;
    result.push({...entry,id:safeText(entry.id),title:text(entry.title),text:body});
  }
  return result.reverse();
}
