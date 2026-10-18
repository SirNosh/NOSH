/**
 * The research map as a laid-out graph: one root (Mission, Direction or Autoresearch) becomes a Mermaid flowchart,
 * beautiful-mermaid lays it out as Unicode box drawing (no browser), and each box is found again on the character
 * grid by its "#k" tag so the TUI can color it, select it, and offer the command its node needs. A nested Autoresearch
 * is one summary box whose experiment tree opens as its own graph: laying out every experiment of a Mission took ~0.5 s.
 */
import { renderMermaidASCII } from 'beautiful-mermaid';
import { MAP_ROOTS, mapGlyph, mapTone, progressBar, record, text, type MapTone } from './view-model.js';

export interface GraphBox { node: Record<string, unknown>; owner: Record<string, unknown> | null; index: number; parent: number | null; opens: boolean; tone: MapTone; top: number; left: number; bottom: number; right: number }
export interface GraphLayout { lines: string[]; boxes: GraphBox[] }
export type GraphDirection = 'up' | 'down' | 'left' | 'right';

const LABEL = 26;
const layouts = new Map<string, GraphLayout>();

/** One terminal cell per character: no astral (emoji), wide, zero-width or combining characters, and nothing Mermaid parses inside a label. */
function cell(value: unknown, limit = LABEL): string {
  const plain = text(value, 400).replace(/[\u{10000}-\u{10FFFF}\u0300-\u036f\u1100-\u115f\u200b-\u200f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe00-\ufe0f\ufe30-\ufe4f\uff00-\uff60\uffe0-\uffe6]/gu, '')
    .replaceAll('"', "'").replaceAll('`', "'").replaceAll('<', '‹').replaceAll('>', '›').replace(/\s+/g, ' ').trim();
  return plain.length > limit ? `${plain.slice(0, limit - 1)}…` : plain;
}
/** A title on up to two box lines, broken at a word where one fits. */
function title(value: unknown): string {
  const whole = cell(value, 400); if (whole.length <= LABEL) return whole;
  const space = whole.lastIndexOf(' ', LABEL); const cut = space > LABEL / 2 ? space : LABEL;
  return `${whole.slice(0, cut).trim()}<br>${cell(whole.slice(cut).trim())}`;
}
/** Progress (▰▰▰▰▱▱ 4/6), a score ("… score 0.0050378 (+0.0136)"), an attempt ("attempt 8/8") or a version, short enough for a box. */
function fact(node: Record<string, unknown>): string {
  const progress = record(node.progress); if (typeof progress.total === 'number' && progress.total > 0) return progressBar(Number(progress.done), progress.total, 6);
  const detail = text(node.detail, 200);
  const score = /(?:score|best) (-?[\d.e-]+)/.exec(detail)?.[1];
  if (score && Number.isFinite(Number(score))) return String(Number(Number(score).toPrecision(4)));
  return /attempt \d+\/\d+/.exec(detail)?.[0] ?? /^v\d+$/.exec(detail)?.[0] ?? '';
}

export function layoutGraph(root: unknown): GraphLayout {
  const value = record(root);
  if (!value.kind) return { lines: [], boxes: [] };
  const nodes: Array<{ node: Record<string, unknown>; owner: Record<string, unknown> | null; parent: number | null; opens: boolean }> = [];
  const source = ['flowchart TD'];
  const visit = (node: Record<string, unknown>, parent: number | null, owner: Record<string, unknown> | null): void => {
    const index = nodes.length; const kind = text(node.kind); const state = text(node.state, 40);
    const all = Array.isArray(node.children) ? node.children.map(record) : [];
    const experiments = all.filter(child => text(child.kind) === 'experiment').length;
    const opens = kind === 'autoresearch' && index > 0 && experiments > 0;
    nodes.push({ node, owner, parent, opens });
    const tag = [`#${index}`, MAP_ROOTS[kind]?.label ?? (kind === 'job' || kind === 'worker' ? kind.toUpperCase() : '')].filter(Boolean).join(' ');
    const status = cell([`${mapGlyph(kind, state)} ${state}`, fact(node), opens ? `${experiments} exp ▸` : ''].filter(Boolean).join(' · '), opens ? LABEL + 8 : LABEL);
    source.push(`  n${index}["${tag}<br>${title(node.title) || cell(node.id)}${status ? `<br>${status}` : ''}"]`);
    if (parent !== null) source.push(`  n${parent} --> n${index}`);
    const nextOwner = kind === 'mission' || kind === 'direction' ? node : owner;
    const children = opens ? [] : all;
    // An Autoresearch's first experiments start from its baseline, as in the Mermaid export.
    let base = index;
    if (kind === 'autoresearch' && children.some(child => text(child.kind) === 'experiment')) {
      base = nodes.length; nodes.push({ node: { kind: 'baseline', title: 'baseline', state: '', detail: '' }, owner: nextOwner, parent: index, opens: false });
      const baseline = /baseline (-?[\d.e-]+)/.exec(text(node.detail, 200))?.[1];
      source.push(`  n${base}["#${base}<br>baseline${baseline ? `<br>${cell(String(Number(Number(baseline).toPrecision(4))))}` : ''}"]`, `  n${index} --> n${base}`);
    }
    for (const child of children) visit(child, text(child.kind) === 'experiment' ? base : index, nextOwner);
  };
  visit(value, null, null);
  const key = source.join('\n');
  const cached = layouts.get(key); if (cached) return cached;
  const raw = renderMermaidASCII(key, { colorMode: 'none', paddingX: 3, paddingY: 3, boxBorderPadding: 0 }).split('\n');
  const width = Math.max(...raw.map(line => line.length));
  const lines = raw.map(line => line.padEnd(width));
  const boxes: GraphBox[] = [];
  for (let top = 0; top < lines.length; top++) for (let left = 0; left < width; left++) {
    if (lines[top]![left] !== '┌') continue;
    let right = left + 1; while (right < width && /[─┬┴┼]/.test(lines[top]![right]!)) right++;
    let bottom = top + 1; while (bottom < lines.length && /[│├┤┼]/.test(lines[bottom]![left]!)) bottom++;
    if (lines[top]![right] !== '┐' || lines[bottom]?.[left] !== '└' || lines[bottom]![right] !== '┘') continue;
    const tag = /#(\d+)/.exec(lines.slice(top + 1, bottom).map(line => line.slice(left + 1, right)).join(' '))?.[1];
    const entry = tag === undefined ? undefined : nodes[Number(tag)];
    if (entry) boxes.push({ ...entry, index: Number(tag), tone: entry.node.kind === 'baseline' ? 'idle' : mapTone(text(entry.node.state, 40)), top, left, bottom, right });
  }
  boxes.sort((a, b) => a.index - b.index);
  const layout = { lines, boxes };
  if (layouts.size >= 16) layouts.delete(layouts.keys().next().value!);
  layouts.set(key, layout);
  return layout;
}

/** The nearest box in a direction (cells are about twice as tall as wide); the current box at an edge. */
export function moveSelection(boxes: GraphBox[], current: number, direction: GraphDirection): number {
  const from = boxes[current]; if (!from) return current;
  const centre = (box: GraphBox) => ({ y: (box.top + box.bottom) / 2 * 2, x: (box.left + box.right) / 2 });
  const origin = centre(from); let best = current; let bestScore = Infinity;
  boxes.forEach((box, index) => {
    if (index === current) return;
    const point = centre(box); const dy = point.y - origin.y; const dx = point.x - origin.x;
    const [along, across] = direction === 'down' ? [dy, dx] : direction === 'up' ? [-dy, dx] : direction === 'right' ? [dx, dy] : [-dx, dy];
    if (along <= 0) return;
    const score = along + 2 * Math.abs(across);
    if (score < bestScore) { bestScore = score; best = index; }
  });
  return best;
}

/** The staging command a person needs for this node, if any: a retry for a failed or blocked graph node, a resume for a blocked root, a job's details. */
export function suggestedCommand(box: GraphBox): string {
  const kind = text(box.node.kind); const state = text(box.node.state, 40); const id = text(box.node.id, 80);
  if (kind === 'node' && ['failed', 'blocked'].includes(state) && box.owner && typeof box.owner.version === 'number') return `/retry ${text(box.owner.kind)}s ${text(box.owner.id, 80)} ${box.owner.version} ${id}`;
  if ((kind === 'mission' || kind === 'direction') && state === 'blocked' && typeof box.node.version === 'number') {
    // Resuming while a node is still failed re-blocks at once (no ready frontier), so retry that node first.
    const stuck = (Array.isArray(box.node.children) ? box.node.children.map(record) : []).find(child => text(child.kind) === 'node' &&['failed', 'blocked'].includes(text(child.state, 40)));
    if (stuck) return `/retry ${kind}s ${id} ${box.node.version} ${text(stuck.id, 80)}`;
    return `/transition ${kind}s ${id} ${box.node.version} ${kind === 'mission' ? 'running' : 'active'}`;
  }
  if (kind === 'job' && id) return `/job ${id}`;
  return '';
}
