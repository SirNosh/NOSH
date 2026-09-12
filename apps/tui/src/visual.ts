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
  if (/wait|approv|pending|pause|review/i.test(state)) return theme.warning;
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
          paddingY: entry.kind === 'user' ? 1 : 0, border: accent ? ['left'] : false,
          customBorderChars: splitBorder, borderColor: tint,
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
      // Existing renderables are moved, never destroyed, when ephemeral text settles.
      shown.forEach((entry, index) => root.add(blocks.get(entry.id)!.box, index));
      order = nextOrder;
    }
  }
  return { root, update };
}

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
        const heading = text(renderer, 'section-' + index, row.section.toUpperCase(), theme.accent);
        heading.marginTop = index ? 1 : 0; heading.marginBottom = 1; heading.attributes = TextAttributes.BOLD;
        root.add(heading); section = row.section;
      }
      const card = new BoxRenderable(renderer, { id: 'record-' + index, width: '100%', flexShrink: 0,
        flexDirection: 'column', backgroundColor: theme.panel, padding: 1, marginBottom: 1,
        border: ['left'], customBorderChars: splitBorder, borderColor: stateColor(row.state) });
      card.add(text(renderer, 'record-title-' + index, row.title));
      if (row.state || row.version) card.add(text(renderer, 'record-state-' + index,
        [row.state, row.version ? `v${row.version}` : ''].filter(Boolean).join(' · '), stateColor(row.state)));
      if (row.subtitle) card.add(text(renderer, 'record-subtitle-' + index, row.subtitle, theme.muted));
      for (const [n, field] of row.fields.entries()) card.add(text(renderer, `field-${index}-${n}`, `${field.label}  ${field.value}`, theme.muted));
      root.add(card);
    }
    if (!rows.length) root.add(text(renderer, 'empty-inspector', fallback, theme.muted));
  }
  return { root, update };
}

export function createResearchSidebar(renderer: CliRenderer) {
  const root = new BoxRenderable(renderer, { id: 'research-sidebar', width: '100%', flexShrink: 0, flexDirection: 'column' });
  let previous = '';
  function update(rows: ViewRow[]) {
    const signature = JSON.stringify(rows);
    if (signature === previous) return;
    previous = signature;
    for (const child of root.getChildren()) { root.remove(child); child.destroyRecursively(); }
    let section = '';
    for (const [i, row] of rows.slice(0, 30).entries()) {
      if (row.section !== section) {
        const heading = text(renderer, 'side-section-' + i, row.section, theme.text);
        heading.attributes = TextAttributes.BOLD; heading.marginTop = 1;
        root.add(heading); section = row.section;
      }
      root.add(text(renderer, 'side-title-' + i, `${row.state ? '• ' : ''}${row.title}`, row.state ? stateColor(row.state) : theme.muted));
      if (row.state) root.add(text(renderer, 'side-state-' + i, `  ${row.state}${row.version ? ' · v' + row.version : ''}`, theme.muted));
      if (row.subtitle) root.add(text(renderer, 'side-subtitle-' + i, row.subtitle, theme.muted));
      for (const [n, field] of row.fields.slice(0, 3).entries()) root.add(text(renderer, `side-field-${i}-${n}`, `${field.label}  ${field.value}`, theme.muted));
    }
    if (!rows.length) {
      for (const [i, [title, hint]] of [
        ['Research', 'Start a conversation to shape your project.'],
        ['Missions & directions', 'No research graph loaded.'],
        ['Supervised jobs', 'Use /jobs to inspect work.'],
        ['Approvals', 'You keep control of research changes.'],
      ].entries()) {
        const heading = text(renderer, 'side-empty-' + i, title!); heading.marginTop = 1; root.add(heading);
        root.add(text(renderer, 'side-hint-' + i, hint!, theme.muted));
      }
    }
  }
  return { root, update };
}
