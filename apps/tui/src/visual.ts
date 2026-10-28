/**
 * Message spacing and left-accent user blocks adapted from OpenCode session UI.
 * Copyright (c) 2025 opencode. MIT license and pinned source: ../UPSTREAM.md.
 */
import { BoxRenderable, TextRenderable, MarkdownRenderable, TextAttributes, StyledText, fg, bold,
  type CliRenderer, type SyntaxStyle } from '@opentui/core';
import { safeText, type MapLine, type MapTone, type TranscriptEntry, type ViewRow } from './controller.js';
import { MAP_ROOTS, record, text as plain } from './view-model.js';
import { boxAt, layoutGraph, moveSelection, shrinkGraph, suggestedCommand, type GraphBox, type GraphDetail, type GraphDirection } from './map-graph.js';
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
  function update(rows: ViewRow[], attention: AttentionItem[] = [], climb: MapLine[] = []) {
    const signature = JSON.stringify([rows, attention, climb]);
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
    // The live climb as a graph: what each kept step built on and what tried to beat it (climbLines).
    for (const [i, line] of climb.entries()) {
      const tone = MAP_TONES[line.tone];
      const row = new TextRenderable(renderer, { id: 'side-climb-' + i, width: '100%', height: 1, wrapMode: 'none', flexShrink: 0 });
      row.content = new StyledText([fg(theme.muted)(line.prefix), fg(tone)(line.glyph ? `${line.glyph} ` : ''),
        ...(line.label ? [bold(fg(theme.accent)(`${line.label} `))] : []), fg(line.heading ? tone : line.glyph ? theme.text : theme.muted)(line.title), fg(tone)(line.state)]);
      if (line.heading) row.marginTop = 1;
      root.add(row);
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

const MAP_TONES: Record<MapTone, string> = { success: theme.success, active: theme.secondary, waiting: theme.warning, error: theme.error, idle: theme.muted };
const EMPTY_MAP = 'Nothing is running yet.\nMissions, research directions, autoresearch experiments and workers appear here, live, as they start.\nAsk NOSH in the conversation to propose research, or /create one.';

const clipped = (value: string, width: number) => value.length > width ? `${value.slice(0, Math.max(1, width - 1))}…` : value;

/**
 * The research map: one research root at a time as a laid-out graph (map-graph.ts), panned to keep the selected box in
 * view, with the selected node named underneath and the one command it needs. Rebuilt only when what is drawn changes.
 */
// Zoom levels, like an image: in to whole titles, the normal boxes, then out where each cell is a factor-by-factor block.
const ZOOMS: Array<{ label: string; detail: GraphDetail; factor: number }> = [
  { label: '150%', detail: 'full', factor: 1 }, { label: '100%', detail: 'normal', factor: 1 },
  { label: '50%', detail: 'normal', factor: 2 }, { label: '25%', detail: 'normal', factor: 4 }, { label: '12%', detail: 'normal', factor: 8 }];

export function createResearchMap(renderer: CliRenderer, changed: () => void) {
  const root = new BoxRenderable(renderer, { id: 'research-map', width: '100%', flexDirection: 'column', flexShrink: 0 });
  // The canvas rows persist (only their content changes): a mouse drag stays captured by the row it started on.
  const header = new BoxRenderable(renderer, { id: 'map-header', width: '100%', flexDirection: 'column', flexShrink: 0 });
  const canvas = new BoxRenderable(renderer, { id: 'map-canvas', width: '100%', flexDirection: 'column', flexShrink: 0 });
  const panel = new BoxRenderable(renderer, { id: 'map-panel', width: '100%', flexDirection: 'column', flexShrink: 0 });
  root.add(header); root.add(canvas); root.add(panel);
  const rowPool: TextRenderable[] = [];
  // Pan is in layout cells; the view starts at the origin (pan snapped to the zoom factor).
  let previous = ''; let rootIndex = 0; let selectedId = ''; let panX = 0; let panY = 0; let originX = 0; let originY = 0;
  let zoom = 1; let follow = true; let fits = true; let graph: unknown;
  let drag: { x: number; y: number; panX: number; panY: number; moved: boolean } | null = null;
  // Autoresearch boxes opened from a Mission or Direction graph, innermost last.
  const opened: string[] = [];
  let shown: { boxes: GraphBox[]; selected: number } = { boxes: [], selected: 0 };
  const find = (node: Record<string, unknown>, id: string): Record<string, unknown> | undefined => node.id === id ? node : (Array.isArray(node.children) ? node.children.map(record) : []).map(child => find(child, id)).find(Boolean);
  const replace = (box: BoxRenderable, children: TextRenderable[]) => { for (const child of box.getChildren()) { box.remove(child); child.destroyRecursively(); } for (const child of children) box.add(child); };
  const line = (id: string, content: StyledText) => new TextRenderable(renderer, { id, content, width: '100%', height: 1, wrapMode: 'none', flexShrink: 0 });
  function setRows(count: number) {
    while (rowPool.length < count) { const row = new TextRenderable(renderer, { id: `map-row-${rowPool.length}`, content: '', width: '100%', height: 1, wrapMode: 'none', flexShrink: 0, selectable: false }); rowPool.push(row); canvas.add(row); }
    while (rowPool.length > count) { const row = rowPool.pop()!; canvas.remove(row); row.destroyRecursively(); }
  }
  function update(map: unknown, width: number, height: number) {
    const roots = Array.isArray(record(map).roots) ? (record(map).roots as unknown[]).map(record) : [];
    rootIndex = roots.length ? (rootIndex % roots.length + roots.length) % roots.length : 0;
    const top = roots[rootIndex];
    while (top && opened.length && !find(top, opened.at(-1)!)) opened.pop();
    const drilled = top && opened.length ? find(top, opened.at(-1)!) : undefined;
    graph = drilled ?? top;
    const level = ZOOMS[zoom]!; const factor = level.factor;
    const layout = layoutGraph(graph, level.detail); const { boxes } = layout;
    const found = selectedId ? boxes.findIndex(box => plain(box.node.id, 80) === selectedId) : -1;
    const selected = Math.max(0, found); selectedId = plain(boxes[selected]?.node.id, 80);
    shown = { boxes, selected };
    const view = factor === 1 ? { lines: layout.lines, owners: null } : shrinkGraph(layout, factor);
    // Live workers outside any graph node (the chat agent): one line under the path.
    const workers = (Array.isArray(record(map).workers) ? (record(map).workers as unknown[]).map(record) : []).map(worker => `⚙ ${plain(worker.title, 40)} ${plain(worker.state, 40)}`.trim());
    // Path line, the workers line, the canvas, then three lines naming the selected node.
    const rows = Math.max(4, height - 5 - (workers.length ? 1 : 0)); const cols = Math.max(12, width);
    // The view spans cols x rows cells, each covering factor x factor layout cells.
    const spanX = cols * factor; const spanY = rows * factor; const canvasWidth = layout.lines[0]?.length ?? 0;
    const box = boxes[selected];
    if (box && follow) {
      if (box.left < panX) panX = Math.max(0, box.left - 2); else if (box.right >= panX + spanX) panX = Math.max(0, Math.min(box.left - 2, box.right - spanX + 3));
      if (box.top < panY) panY = Math.max(0, box.top - 1); else if (box.bottom >= panY + spanY) panY = Math.max(0, Math.min(box.top - 1, box.bottom - spanY + 2));
    }
    panX = Math.max(0, Math.min(panX, canvasWidth - spanX)); panY = Math.max(0, Math.min(panY, layout.lines.length - spanY));
    fits = canvasWidth <= spanX && layout.lines.length <= spanY;
    const x0 = Math.floor(panX / factor); const y0 = Math.floor(panY / factor); originX = x0 * factor; originY = y0 * factor;
    const signature = JSON.stringify([view.lines, boxes.map(entry => entry.tone), selected, x0, y0, zoom, width, height, rootIndex, opened, roots.length, workers]);
    if (signature === previous) return;
    previous = signature;
    // Workers alone (the chat agent) are not research; say how research starts.
    if (!top) { const empty = text(renderer, 'map-empty', EMPTY_MAP, theme.muted); empty.marginBottom = 1; replace(header, [empty]); setRows(0); replace(panel, []); return; }
    const kind = plain(top.kind);
    const path = [MAP_ROOTS[kind]?.label ?? kind.toUpperCase(), `${rootIndex + 1}/${roots.length}`, plain(top.state), plain(top.title, 200), ...(drilled ? [`› ${plain(drilled.title, 80)}`] : [])].join('  ');
    // Keys and mouse are in the footer; the header keeps room for the path.
    const hint = `${level.label}${roots.length > 1 ? ' · tab next' : ''}${opened.length ? ' · backspace back' : ''}`;
    replace(header, [line('map-path', new StyledText([bold(fg(theme.accent)(clipped(path, Math.max(8, cols - hint.length - 2)))), fg(theme.muted)(`  ${hint}`)])),
      ...(workers.length ? [line('map-workers', new StyledText([fg(theme.secondary)(clipped(`workers  ${workers.join('  ·  ')}`, cols))]))] : [])]);
    // Each cell takes its box's state color (the selected box the accent, bold); edges between boxes stay muted.
    const visible = Math.max(0, Math.min(rows, view.lines.length - y0)); setRows(visible);
    for (let index = 0; index < visible; index++) {
      const row = y0 + index; const cells = view.lines[row]!.slice(x0, x0 + cols); const chunks: ReturnType<ReturnType<typeof fg>>[] = [];
      const ownerAt = (column: number) => view.owners ? view.owners[row]![column + x0] ?? -1 : boxAt(boxes, column + x0, row);
      let start = 0; let owner = ownerAt(0);
      for (let column = 1; column <= cells.length; column++) {
        const next = column < cells.length ? ownerAt(column) : -2;
        if (next === owner) continue;
        const piece = fg(owner === -1 ? theme.muted : owner === selected ? theme.accent : MAP_TONES[boxes[owner]!.tone])(cells.slice(start, column));
        chunks.push(owner === selected ? bold(piece) : piece); start = column; owner = next;
      }
      rowPool[index]!.content = new StyledText(chunks);
    }
    if (!box) { replace(panel, []); return; }
    const command = suggestedCommand(box);
    // Siblings can share a connector line, so the panel names the box this one hangs from.
    const parent = boxes.find(entry => entry.index === box.parent);
    const facts = [plain(box.node.kind) === 'node' ? 'graph node' : plain(box.node.kind), plain(box.node.state, 40), plain(box.node.detail, 200).replace(/(\d\.\d{5})\d+/g, '$1'), plain(box.node.id, 80), parent ? `under #${parent.index} ${clipped(plain(parent.node.title, 200), 40)}` : ''].filter(Boolean).join(' · ');
    const action = box.opens ? 'enter  open its experiments' : command ? `enter  ${command}   (fills the composer; nothing changes until you confirm)` : '';
    const name = line('map-selected', new StyledText([bold(fg(theme.text)(clipped(plain(box.node.title, 400) || plain(box.node.id, 80), cols)))])); name.marginTop = 1;
    replace(panel, [name, line('map-selected-facts', new StyledText([fg(MAP_TONES[box.tone])(clipped(facts, cols))])),
      ...(action ? [line('map-selected-action', new StyledText([fg(theme.accent)(clipped(action, cols))]))] : [])]);
  }
  const select = (index: number) => { selectedId = plain(shown.boxes[index]?.node.id, 80); };
  /** Zoom to another level, keeping the point under the pointer in place (across layouts: the box under it). */
  function zoomTo(next: number, cell: { x: number; y: number }) {
    const from = ZOOMS[zoom]!; const to = ZOOMS[next]!;
    const point = { x: originX + cell.x * from.factor, y: originY + cell.y * from.factor };
    if (from.detail === to.detail) { panX = point.x - cell.x * to.factor; panY = point.y - cell.y * to.factor; }
    else {
      const before = layoutGraph(graph, from.detail).boxes; const after = layoutGraph(graph, to.detail).boxes;
      const centre = (box: GraphBox) => Math.abs((box.left + box.right) / 2 - point.x) + 2 * Math.abs((box.top + box.bottom) / 2 - point.y);
      const hit = boxAt(before, point.x, point.y); const anchor = hit >= 0 ? before[hit] : [...before].sort((a, b) => centre(a) - centre(b))[0];
      const target = after.find(box => box.index === anchor?.index);
      if (anchor && target) {
        panX = target.left + (point.x - anchor.left) * (target.right - target.left) / Math.max(1, anchor.right - anchor.left) - cell.x;
        panY = target.top + (point.y - anchor.top) * (target.bottom - target.top) / Math.max(1, anchor.bottom - anchor.top) - cell.y;
      }
    }
    panX = Math.max(0, Math.round(panX)); panY = Math.max(0, Math.round(panY)); zoom = next; follow = false;
  }
  // Mouse, like an image viewer: the wheel zooms at the pointer, a drag moves the view, a click selects a box.
  // Each handler stops the event so the conversation behind the map neither scrolls nor takes focus.
  const cellAt = (event: { x: number; y: number }) => ({ x: event.x - canvas.x, y: event.y - canvas.y });
  canvas.onMouseScroll = (event) => {
    event.stopPropagation();
    const direction = event.scroll?.direction;
    if (direction === 'up' && zoom > 0) zoomTo(zoom - 1, cellAt(event));
    else if (direction === 'down' && zoom < ZOOMS.length - 1 && !fits) zoomTo(zoom + 1, cellAt(event));
    else return;
    changed();
  };
  canvas.onMouseDown = (event) => { if (event.button !== 0) return; event.preventDefault(); event.stopPropagation(); drag = { x: event.x, y: event.y, panX: originX, panY: originY, moved: false }; };
  canvas.onMouseDrag = (event) => {
    if (!drag) return; event.stopPropagation();
    const factor = ZOOMS[zoom]!.factor; drag.moved ||= event.x !== drag.x || event.y !== drag.y;
    panX = Math.max(0, drag.panX - (event.x - drag.x) * factor); panY = Math.max(0, drag.panY - (event.y - drag.y) * factor); follow = false;
    changed();
  };
  canvas.onMouseUp = (event) => {
    if (!drag) return; event.stopPropagation();
    const click = !drag.moved; drag = null; if (!click) return;
    const factor = ZOOMS[zoom]!.factor; const cell = cellAt(event); const index = boxAt(shown.boxes, originX + cell.x * factor, originY + cell.y * factor);
    if (index < 0) return;
    select(index); follow = false; changed();
  };
  return {
    root, update,
    move(direction: GraphDirection) { select(moveSelection(shown.boxes, shown.selected, direction)); follow = true; },
    cycle(step: number) { rootIndex += step; opened.length = 0; selectedId = ''; panX = 0; panY = 0; follow = true; },
    pan(rows: number) { panY = Math.max(0, panY + rows * ZOOMS[zoom]!.factor); follow = false; },
    /** Enter on the selected box: open a nested Autoresearch (''), else the command its node needs ('' when none). */
    open(): string {
      const box = shown.boxes[shown.selected]; if (!box) return '';
      if (box.opens) { opened.push(plain(box.node.id, 80)); selectedId = ''; panX = 0; panY = 0; follow = true; return ''; }
      return suggestedCommand(box);
    },
    back(): boolean { if (!opened.length) return false; selectedId = opened.pop()!; follow = true; return true; },
  };
}
