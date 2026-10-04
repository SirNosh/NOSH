/**
 * Message spacing and left-accent user blocks adapted from OpenCode session UI.
 * Copyright (c) 2025 opencode. MIT license and pinned source: ../UPSTREAM.md.
 */
import { BoxRenderable, TextRenderable, MarkdownRenderable, TextAttributes,
  type CliRenderer, type SyntaxStyle } from '@opentui/core';
import { safeText, type TranscriptEntry, type ViewRow } from './controller.js';
import { theme, splitBorder } from './theme.js';

export function text(renderer: CliRenderer, id: string, content: string, color: string = theme.text): TextRenderable {
  return new TextRenderable(renderer, { id, content: safeText(content), fg: color,
    width: '100%', wrapMode: 'word', flexShrink: 0 });
}

function stateColor(state = ''): string {
  if (/fail|error|reject|cancel|stop/i.test(state)) return theme.error;
  if (/approved|accept/i.test(state)) return theme.success;
  if (/wait|approv|pending|pause|review|draft/i.test(state)) return theme.warning;
  if (/complete|success|ready|accept|connected/i.test(state)) return theme.success;
  if (/running|working|active|stream/i.test(state)) return theme.secondary;
  return theme.muted;
}

/** Reconcile messages by identity: streaming updates do not reset scroll or selection. */
export function createTranscript(renderer: CliRenderer, syntax: SyntaxStyle) {
  const root = new BoxRenderable(renderer, { id: 'transcript', width: '100%', flexDirection: 'column', flexShrink: 0 });
  const blocks = new Map<string, { box: BoxRenderable; title: TextRenderable; body: TextRenderable | MarkdownRenderable; signature: string }>();
  let order = '';
  function update(entries: TranscriptEntry[]) {
    // A bounded native scene complements the controller's bounded replay window.
    const shown = entries.slice(-100);
    const nextOrder = shown.map(entry => entry.id).join('\0');
    const ids = new Set(shown.map(entry => entry.id));
    for (const [id, block] of blocks) if (!ids.has(id)) {
      root.remove(block.box); block.box.destroyRecursively(); blocks.delete(id);
    }
    for (const entry of shown) {
      const signature = JSON.stringify(entry);
      let block = blocks.get(entry.id);
      if (block?.signature === signature) continue;
      const tint = entry.kind === 'user' ? theme.accent : entry.kind === 'failure' ? theme.error
        : entry.kind === 'approval' ? theme.warning : entry.kind === 'tool' ? theme.cyan
        : entry.kind === 'receipt' ? theme.success : theme.secondary;
      if (!block) {
        const accent = ['user', 'failure', 'approval'].includes(entry.kind);
        const box = new BoxRenderable(renderer, { id: 'message-' + entry.id, width: '100%',
          flexDirection: 'column', flexShrink: 0, marginBottom: 1, paddingX: 2,
          paddingY: entry.kind === 'user' ? 1 : 0,
          // Custom border characters enable a full border even with border:false; pass them only with the left accent.
          ...(accent ? { border: ['left'] as ('left')[], customBorderChars: splitBorder, borderColor: tint } : {}),
          backgroundColor: accent ? theme.panel : theme.background });
        const title = text(renderer, 'label-' + entry.id, '', tint);
        title.attributes = TextAttributes.BOLD;
        const body = entry.kind === 'assistant' ? new MarkdownRenderable(renderer, {
          id: 'body-' + entry.id, width: '100%', content: '', fg: theme.text, syntaxStyle: syntax,
          conceal: true, streaming: entry.status === 'streaming',
          tableOptions: { style: 'columns', borders: false, wrapMode: 'word' },
        }) : text(renderer, 'body-' + entry.id, '', entry.kind === 'tool' || entry.kind === 'receipt' ? theme.muted : theme.text);
        box.add(title); box.add(body); root.add(box);
        block = { box, title, body, signature: '' }; blocks.set(entry.id, block);
      }
      const icon = { user: '▪', assistant: '◆', tool: '↳', failure: '!', receipt: '✓', approval: '?' }[entry.kind];
      const meta = [entry.model, entry.thinkingLevel, entry.status === 'streaming' ? 'writing…' : undefined].filter(Boolean).join(' · ');
      block.title.content = safeText(`${icon} ${entry.title}${meta ? '  ·  ' + meta : ''}`);
      block.body.content = safeText(entry.text);
      if (block.body instanceof MarkdownRenderable) block.body.streaming = entry.status === 'streaming';
      block.signature = signature;
    }
    if (order !== nextOrder) {
      // add(child, index) inserts before the child currently at index; it is not
      // an idempotent assignment. Never ask OpenTUI to insert a node before itself.
      // Read the current order after each move, preserving existing renderables
      // (and their selection/layout state) when ephemeral text settles.
      shown.forEach((entry, index) => {
        const box = blocks.get(entry.id)!.box;
        if (root.getChildren()[index] !== box) root.add(box, index);
      });
      order = nextOrder;
    }
  }
  return { root, update };
}

const SECTION_TITLES: Record<string, string> = { missions: 'Missions', directions: 'Directions', autoresearch: 'Autoresearch',
  agents: 'Agents', jobs: 'Jobs', approvals: 'Approvals', contract: 'Project contract', projects: 'Projects', models: 'Models',
  action: 'Staged action', result: 'Result', output: 'Job output' };
function sectionTitle(section: string): string { return SECTION_TITLES[section] ?? section.charAt(0).toUpperCase() + section.slice(1); }
// Rows without an entity id get "<section>-<index>"; that is not a usable command argument.
function commandId(row: ViewRow): string {
  const placeholder = row.section === 'action' || row.id === row.section || row.id.startsWith(row.section + '-') && /^\d+$/.test(row.id.slice(row.section.length + 1));
  return row.id !== row.title && !placeholder ? row.id : '';
}
const MAX_CARD_FIELDS = 8;

/** State is a readable list of cards and fields, never a JSON document dump. */
export function createInspector(renderer: CliRenderer) {
  const root = new BoxRenderable(renderer, { id: 'inspector', width: '100%', flexDirection: 'column', flexShrink: 0 });
  let previous = '';
  function update(rows: ViewRow[], fallback: string) {
    const signature = JSON.stringify([rows, fallback]);
    if (signature === previous) return;
    previous = signature;
    for (const child of root.getChildren()) { root.remove(child); child.destroyRecursively(); }
    let section = '';
    for (const [index, row] of rows.slice(0, 120).entries()) {
      if (row.section !== section) {
        const count = rows.filter(r => r.section === row.section).length;
        const heading = text(renderer, 'section-' + index, `${sectionTitle(row.section)}${count > 1 ? `  (${count})` : ''}`, theme.accent);
        heading.marginTop = index ? 1 : 0; heading.marginBottom = 1; heading.attributes = TextAttributes.BOLD;
        root.add(heading); section = row.section;
      }
      const card = new BoxRenderable(renderer, { id: 'record-' + index, width: '100%', flexShrink: 0,
        flexDirection: 'column', backgroundColor: theme.panel, padding: 1, marginBottom: 1,
        border: ['left'], customBorderChars: splitBorder, borderColor: stateColor(row.state) });
      if (row.section !== 'output') {
        const title = text(renderer, 'record-title-' + index, row.title); title.attributes = TextAttributes.BOLD;
        card.add(title);
      }
      if (row.state || row.version) card.add(text(renderer, 'record-state-' + index,
        [row.state, row.version ? `v${row.version}` : ''].filter(Boolean).join(' · '), stateColor(row.state)));
      // Staged commands need the exact id; show it where it can be copied.
      const id = commandId(row);
      if (id) card.add(text(renderer, 'record-id-' + index, `id ${id}`, theme.muted));
      if (row.subtitle) card.add(text(renderer, 'record-subtitle-' + index, row.subtitle, theme.muted));
      // Title and subtitle are already shown; do not repeat them as fields.
      const fields = row.fields.filter(field => row.section === 'output' || field.value !== row.title && field.value !== row.subtitle && field.value !== id);
      const shown = fields.slice(0, row.section === 'output' ? 1 : MAX_CARD_FIELDS);
      const labelWidth = Math.min(26, Math.max(10, ...shown.map(field => field.label.length + 1)));
      for (const [n, field] of shown.entries()) {
        if (row.section === 'output') { card.add(text(renderer, `field-${index}-${n}`, field.value)); continue; }
        const line = new BoxRenderable(renderer, { id: `field-${index}-${n}`, width: '100%', flexDirection: 'row', flexShrink: 0, gap: 2 });
        const label = text(renderer, `field-label-${index}-${n}`, field.label, theme.muted);
        label.width = labelWidth; label.flexShrink = 0; label.wrapMode = 'none';
        const value = text(renderer, `field-value-${index}-${n}`, field.value); value.width = 'auto'; value.flexGrow = 1; value.flexShrink = 1; value.minWidth = 0;
        line.add(label); line.add(value); card.add(line);
      }
      if (fields.length > shown.length) card.add(text(renderer, `field-more-${index}`, `+${fields.length - shown.length} more fields${row.section === 'jobs' && id ? ` · /job ${id}` : ''}`, theme.muted));
      root.add(card);
    }
    if (!rows.length) root.add(text(renderer, 'empty-inspector', fallback, theme.muted));
  }
  return { root, update };
}

export interface AttentionItem { title: string; hint: string }

/** One line per record, OpenCode style; details live in the inspector views. */
export function createResearchSidebar(renderer: CliRenderer) {
  const root = new BoxRenderable(renderer, { id: 'research-sidebar', width: '100%', flexShrink: 0, flexDirection: 'column' });
  let previous = '';
  function heading(id: string, content: string, color: string = theme.text) {
    const node = text(renderer, id, content, color); node.attributes = TextAttributes.BOLD; node.marginTop = 1; root.add(node);
  }
  function line(id: string, title: string, state: string | undefined) {
    const row = new BoxRenderable(renderer, { id, width: '100%', flexDirection: 'row', flexShrink: 0, gap: 1 });
    const label = text(renderer, id + '-title', `${state ? '●' : '·'} ${title}`, state ? stateColor(state) : theme.muted);
    label.wrapMode = 'none'; label.flexGrow = 1; label.flexShrink = 1; label.minWidth = 0; label.width = 'auto';
    row.add(label);
    if (state) { const tag = text(renderer, id + '-state', state, theme.muted); tag.width = 'auto'; tag.flexShrink = 0; tag.wrapMode = 'none'; row.add(tag); }
    root.add(row);
  }
  function update(rows: ViewRow[], attention: AttentionItem[] = []) {
    const signature = JSON.stringify([rows, attention]);
    if (signature === previous) return;
    previous = signature;
    for (const child of root.getChildren()) { root.remove(child); child.destroyRecursively(); }
    if (attention.length) {
      heading('side-attention', 'Needs you', theme.warning);
      for (const [i, item] of attention.slice(0, 6).entries()) {
        root.add(text(renderer, 'side-attention-' + i, `! ${item.title}`, theme.warning));
        root.add(text(renderer, 'side-attention-hint-' + i, `  ${item.hint}`, theme.muted));
      }
    }
    const sections = [...new Set(rows.map(row => row.section))];
    for (const section of sections) {
      const items = rows.filter(row => row.section === section);
      const running = section === 'jobs' ? items.filter(row => /running|active/i.test(row.state ?? '')).length : 0;
      heading('side-section-' + section, `${sectionTitle(section)}  ${items.length}${running ? ` · ${running} running` : ''}`);
      for (const [i, row] of items.slice(0, 8).entries()) line(`side-${section}-${i}`, row.title, row.state);
      if (items.length > 8) root.add(text(renderer, `side-${section}-more`, `  +${items.length - 8} more`, theme.muted));
    }
    if (!rows.length && !attention.length) {
      for (const [i, [title, hint]] of [
        ['Missions & directions', 'None yet · /status'],
        ['Jobs', 'None yet · /jobs'],
        ['Approvals', 'None · /approvals'],
      ].entries()) {
        heading('side-empty-' + i, title!);
        root.add(text(renderer, 'side-hint-' + i, hint!, theme.muted));
      }
    }
  }
  return { root, update };
}
