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
/** `normal` boxes hold a title on two lines; zoomed in, `full` boxes hold whole titles and the node's detail. */
export type GraphDetail = 'normal' | 'full';
/** A shrunk graph: one Braille glyph per factor-by-factor block of the layout, and the box index (or -1) each glyph belongs to. */
export interface ShrunkGraph { lines: string[]; owners: number[][] }

const LABELS: Record<GraphDetail, { width: number; lines: number }> = { normal: { width: 26, lines: 2 }, full: { width: 40, lines: 3 } };
const LABEL = LABELS.normal.width;
const layouts = new Map<string, GraphLayout>();

/** One terminal cell per character: no astral (emoji), wide, zero-width or combining characters, and nothing Mermaid parses inside a label. */
function cell(value: unknown, limit = LABEL): string {
  const plain = text(value, 400).replace(/[\u{10000}-\u{10FFFF}\u0300-\u036f\u1100-\u115f\u200b-\u200f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe00-\ufe0f\ufe30-\ufe4f\uff00-\uff60\uffe0-\uffe6]/gu, '')
    .replaceAll('"', "'").replaceAll('`', "'").replaceAll('<', '‹').replaceAll('>', '›').replace(/\s+/g, ' ').trim();
  return plain.length > limit ? `${plain.slice(0, limit - 1)}…` : plain;
}
/** A title on up to `lines` box lines of `width`, each broken at a word where one fits; the last line ends in … when cut. */
function title(value: unknown, width = LABEL, lines = 2): string {
  const whole = cell(value, 400); if (whole.length <= width || lines <= 1) return cell(whole, width);
  const space = whole.lastIndexOf(' ', width); const cut = space > width / 2 ? space : width;
  return `${whole.slice(0, cut).trim()}<br>${title(whole.slice(cut).trim(), width, lines - 1)}`;
}
/** Progress (▰▰▰▰▱▱ 4/6), a score ("… score 0.0050378 (+0.0136)"), an attempt ("attempt 8/8") or a version, short enough for a box. */
function fact(node: Record<string, unknown>): string {
  const progress = record(node.progress); if (typeof progress.total === 'number' && progress.total > 0) return progressBar(Number(progress.done), progress.total, 6);
  const detail = text(node.detail, 200);
  const score = /(?:score|best) (-?[\d.e-]+)/.exec(detail)?.[1];
  if (score && Number.isFinite(Number(score))) return String(Number(Number(score).toPrecision(4)));
  return /attempt \d+\/\d+/.exec(detail)?.[0] ?? /^v\d+$/.exec(detail)?.[0] ?? '';
}

export function layoutGraph(root: unknown, detail: GraphDetail = 'normal'): GraphLayout {
  const value = record(root); const label = LABELS[detail];
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
    const status = cell([`${mapGlyph(kind, state)} ${state}`, fact(node), opens ? `${experiments} exp ▸` : ''].filter(Boolean).join(' · '), opens ? label.width + 8 : label.width);
    // Zoomed in, the node's whole detail line (role, attempt, score) follows its state.
    const more = detail === 'full' ? cell(node.detail, label.width) : '';
    source.push(`  n${index}["${tag}<br>${title(node.title, label.width, label.lines) || cell(node.id)}${status ? `<br>${status}` : ''}${more ? `<br>${more}` : ''}"]`);
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

/** The box drawn at a layout cell, or -1. */
export function boxAt(boxes: GraphBox[], x: number, y: number): number {
  return boxes.findIndex(box => x >= box.left && x <= box.right && y >= box.top && y <= box.bottom);
}

// Braille dot bits by [row][column] within a glyph's 2x4 dot grid.
const DOTS = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const shrunk = new WeakMap<GraphLayout, Map<number, ShrunkGraph>>();

/**
 * The layout zoomed out by `factor`, like an image: each output cell covers a factor-by-factor block of layout cells (a
 * cell is about twice as tall as wide, so the picture keeps its shape). A dot is set where its part of the block holds
 * any character, the glyph belongs to the box owning most of its inked cells, and each box keeps its #k tag where it fits.
 */
export function shrinkGraph(layout: GraphLayout, factor: number): ShrunkGraph {
  const cached = shrunk.get(layout)?.get(factor); if (cached) return cached;
  const { lines, boxes } = layout; const height = lines.length; const width = lines[0]?.length ?? 0;
  const ownerOf = lines.map(() => new Int16Array(width).fill(-1));
  boxes.forEach((box, index) => { for (let row = box.top; row <= box.bottom; row++) ownerOf[row]!.fill(index, box.left, box.right + 1); });
  const rows = Math.ceil(height / factor); const cols = Math.ceil(width / factor);
  const glyphs: string[][] = []; const owners: number[][] = [];
  for (let y = 0; y < rows; y++) {
    const glyphRow: string[] = []; const ownerRow: number[] = [];
    for (let x = 0; x < cols; x++) {
      let bits = 0; const votes = new Map<number, number>();
      for (let dy = 0; dy < 4; dy++) for (let dx = 0; dx < 2; dx++) {
        const top = Math.floor(y * factor + dy * factor / 4); const bottom = Math.min(height, Math.max(top + 1, Math.floor(y * factor + (dy + 1) * factor / 4)));
        const left = x * factor + dx * factor / 2; const right = Math.min(width, left + factor / 2);
        for (let row = top; row < bottom; row++) for (let column = left; column < right; column++) {
          if (lines[row]![column] === ' ') continue;
          bits |= DOTS[dy]![dx]!; const owner = ownerOf[row]![column]!; votes.set(owner, (votes.get(owner) ?? 0) + 1);
        }
      }
      glyphRow.push(bits ? String.fromCharCode(0x2800 + bits) : ' ');
      ownerRow.push([...votes].sort((a, b) => b[1] - a[1] || b[0] - a[0])[0]?.[0] ?? -1);
    }
    glyphs.push(glyphRow); owners.push(ownerRow);
  }
  boxes.forEach((box, index) => {
    const tag = `#${box.index}`; const left = Math.ceil(box.left / factor) + 1; const right = Math.floor(box.right / factor) - 1;
    if (right - left + 1 < tag.length) return;
    const row = Math.floor((box.top + box.bottom) / 2 / factor); const start = Math.max(left, Math.floor((left + right + 1 - tag.length) / 2));
    for (let i = 0; i < tag.length; i++) { glyphs[row]![start + i] = tag[i]!; owners[row]![start + i] = index; }
  });
  const result = { lines: glyphs.map(row => row.join('')), owners };
  const byFactor = shrunk.get(layout) ?? new Map<number, ShrunkGraph>(); byFactor.set(factor, result); shrunk.set(layout, byFactor);
  return result;
}
