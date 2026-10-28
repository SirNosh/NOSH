import { describe, expect, it } from 'vitest';
import { boxAt, layoutGraph, moveSelection, shrinkGraph, suggestedCommand, type GraphBox } from './map-graph.js';

// The live shape: a Mission's graph nodes, one of which spawned a Direction whose node spawned an Autoresearch.
const root = {
  kind: 'mission', id: 'mis_x', title: 'Trustworthy spam probabilities', state: 'blocked', detail: 'v113', version: 113, progress: { done: 4, total: 6 },
  children: [
    { kind: 'node', id: 'mnode_dir', title: 'Research Direction 1: answer one question', state: 'accepted', detail: 'research direction', children: [
      { kind: 'direction', id: 'dir_y', title: 'Which calibrator has the "lowest" ECE? 🧪', state: 'blocked', detail: 'v25', version: 25, children: [
        { kind: 'node', id: 'dnode_eval', title: 'Evaluate bounded hypothesis families', state: 'failed', detail: 'general worker · attempt 3/3', children: [
          { kind: 'autoresearch', id: 'ar_z', title: 'How low can calibration push ECE', state: 'completed', detail: 'round 2/4 · best 0.005037866', children: [
            { kind: 'experiment', id: 'exp_a', title: 'Histogram binning', state: 'promoted', detail: 'round 1 · score 0.005037866372912155 (+0.0136)' },
            { kind: 'experiment', id: 'exp_b', title: 'Platt scaling', state: 'rejected', detail: 'round 1 · score 0.0115 (-0.0065)' },
          ] },
        ] },
      ] },
    ] },
    { kind: 'node', id: 'mnode_impl', title: 'Produce the bounded deliverable', state: 'failed', detail: 'implementation · attempt 8/8', children: [] },
    { kind: 'node', id: 'mnode_done', title: 'Collect and cite the sources', state: 'accepted', detail: 'librarian', children: [] },
    { kind: 'job', id: 'job_q', title: 'node --test', state: 'running', detail: 'native' },
  ],
};
const box = (boxes: GraphBox[], id: string) => boxes.find(entry => entry.node.id === id)!;

describe('research map graph', () => {
  it('lays every map node out in exactly one box that holds its title, with no overlapping boxes', () => {
    const { lines, boxes } = layoutGraph(root);
    const ids = boxes.filter(entry => entry.node.id).map(entry => entry.node.id);
    // A nested Autoresearch is one box that opens as its own graph, where its experiments hang from the baseline.
    expect(ids.sort()).toEqual(['ar_z', 'dir_y', 'dnode_eval', 'job_q', 'mis_x', 'mnode_dir', 'mnode_done', 'mnode_impl']);
    expect(box(boxes, 'ar_z').opens).toBe(true); expect(boxes.filter(entry => entry.opens)).toHaveLength(1);
    const opened = layoutGraph(box(boxes, 'ar_z').node);
    expect(opened.boxes.map(entry => entry.node.kind === 'baseline' ? 'baseline' : entry.node.id)).toEqual(['ar_z', 'baseline', 'exp_a', 'exp_b']);
    expect(opened.boxes.some(entry => entry.opens)).toBe(false);
    for (const entry of boxes) {
      expect(lines[entry.top]![entry.left]).toBe('┌'); expect(lines[entry.bottom]![entry.right]).toBe('┘');
      const inside = lines.slice(entry.top + 1, entry.bottom).map(line => line.slice(entry.left + 1, entry.right)).join(' ');
      expect(inside).toContain(`#${entry.index}`);
    }
    const inBox = (id: string) => lines.slice(box(boxes, id).top, box(boxes, id).bottom).map(line => line.slice(box(boxes, id).left, box(boxes, id).right + 1)).join(' ');
    expect(inBox('mnode_impl')).toContain('Produce the bounded'); expect(inBox('mnode_impl')).toContain('✗ failed · attempt 8/8');
    // A long title breaks at a word onto a second line instead of losing its end.
    expect(inBox('mnode_dir')).toContain('Research Direction 1:'); expect(inBox('mnode_dir')).toContain('answer one question');
    // Each box knows the box it hangs from (experiments hang from the baseline).
    expect(box(boxes, 'dir_y').parent).toBe(box(boxes, 'mnode_dir').index);
    expect(layoutGraph(box(boxes, 'ar_z').node).boxes.find(entry => entry.node.id === 'exp_a')!.parent).toBe(1);
    // A root shows its progress as it did in the tree map.
    expect(inBox('mis_x')).toContain('MISSION'); expect(inBox('mis_x')).toContain('▰▰▰▰▱▱ 4/6');
    for (const a of boxes) for (const b of boxes) if (a !== b) expect(a.right < b.left || b.right < a.left || a.bottom < b.top || b.bottom < a.top).toBe(true);
    // Quotes and an emoji in a title neither break the layout nor shift columns (one character, one cell).
    expect(lines.every(line => line.length === lines[0]!.length)).toBe(true);
    expect(lines.join('\n')).not.toMatch(/[\u{1F300}-\u{1FAFF}]/u);
  });
  it('moves the selection to the nearest box in a direction and stays put at an edge', () => {
    const { boxes } = layoutGraph(root);
    const mission = boxes.indexOf(box(boxes, 'mis_x'));
    expect(moveSelection(boxes, mission, 'up')).toBe(mission);
    const below = moveSelection(boxes, mission, 'down');
    expect(['mnode_dir', 'mnode_impl', 'mnode_done', 'job_q']).toContain(boxes[below]!.node.id);
    const right = moveSelection(boxes, boxes.indexOf(box(boxes, 'mnode_dir')), 'right');
    expect(boxes[right]!.left).toBeGreaterThan(box(boxes, 'mnode_dir').left);
  });
  it('suggests the exact staging command a person needs for a failed node or a blocked root, and nothing for finished work', () => {
    const { boxes } = layoutGraph(root);
    expect(suggestedCommand(box(boxes, 'mnode_impl'))).toBe('/retry missions mis_x 113 mnode_impl');
    expect(suggestedCommand(box(boxes, 'dnode_eval'))).toBe('/retry directions dir_y 25 dnode_eval');
    // Resuming a blocked root whose node is still failed re-blocks it at once, so the root offers that node's retry first.
    expect(suggestedCommand(box(boxes, 'mis_x'))).toBe('/retry missions mis_x 113 mnode_impl');
    expect(suggestedCommand(box(boxes, 'dir_y'))).toBe('/retry directions dir_y 25 dnode_eval');
    const retried = layoutGraph({ ...root, children: root.children.filter(child => child.state !== 'failed') }).boxes;
    expect(suggestedCommand(box(retried, 'mis_x'))).toBe('/transition missions mis_x 113 running');
    const direction = { kind: 'direction', id: 'dir_y', title: 'Which calibrator', state: 'blocked', detail: 'v26', version: 26, children: [{ kind: 'node', id: 'dnode_eval', title: 'Evaluate', state: 'ready', detail: 'general worker · attempt 3/4', children: [] }] };
    expect(suggestedCommand(box(layoutGraph(direction).boxes, 'dir_y'))).toBe('/transition directions dir_y 26 active');
    expect(suggestedCommand(box(boxes, 'job_q'))).toBe('/job job_q');
    for (const id of ['mnode_done', 'ar_z']) expect(suggestedCommand(box(boxes, id))).toBe('');
    expect(suggestedCommand(box(layoutGraph(box(boxes, 'ar_z').node).boxes, 'exp_a'))).toBe('');
  });
  it('zooms in to whole titles and the full detail line', () => {
    const full = layoutGraph(root, 'full');
    const words = (layout: ReturnType<typeof layoutGraph>, id: string) => { const entry = box(layout.boxes, id); return layout.lines.slice(entry.top, entry.bottom).map(line => line.slice(entry.left, entry.right + 1)).join(' ').replace(/[│┌┐└┘─┬┴├┤]/g, ' ').replace(/\s+/g, ' '); };
    expect(words(full, 'dnode_eval')).toContain('general worker · attempt 3/3');
    const long = { ...root, children: [{ kind: 'node', id: 'mnode_long', title: 'Compare temperature, Platt and histogram calibration at natural and shifted prevalence', state: 'ready', detail: 'implementation', children: [] }] };
    expect(words(layoutGraph(long), 'mnode_long')).toContain('…');
    expect(words(layoutGraph(long, 'full'), 'mnode_long')).toContain('Compare temperature, Platt and histogram calibration at natural and shifted prevalence');
    expect(box(full.boxes, 'mnode_dir').right - box(full.boxes, 'mnode_dir').left).toBeGreaterThan(box(layoutGraph(root).boxes, 'mnode_dir').right - box(layoutGraph(root).boxes, 'mnode_dir').left);
  });
  it('shrinks the graph like an image: each cell is one Braille glyph for a factor-by-factor block, colored by its box', () => {
    const layout = layoutGraph(root);
    for (const factor of [2, 4, 8]) {
      const shrunk = shrinkGraph(layout, factor);
      expect(shrunk.lines).toHaveLength(Math.ceil(layout.lines.length / factor));
      expect(shrunk.lines.every(line => line.length === Math.ceil(layout.lines[0]!.length / factor))).toBe(true);
      expect(shrunk.owners).toHaveLength(shrunk.lines.length);
      expect(shrinkGraph(layout, factor)).toBe(shrunk);
    }
    const shrunk = shrinkGraph(layout, 2); const mission = box(layout.boxes, 'mis_x'); const index = layout.boxes.indexOf(mission);
    // The box's top-left corner block draws dots and belongs to the box; a block of empty canvas stays blank.
    const corner = shrunk.lines[mission.top / 2 | 0]![mission.left / 2 | 0]!;
    expect(corner.codePointAt(0)).toBeGreaterThan(0x2800); expect(corner.codePointAt(0)).toBeLessThanOrEqual(0x28ff);
    expect(shrunk.owners[mission.top / 2 | 0]![mission.left / 2 | 0]).toBe(index);
    const blank = layout.lines.findIndex((line, row) => row % 2 === 0 && /^ +$/.test(line.slice(0, 2)) && /^ +$/.test(layout.lines[row + 1]?.slice(0, 2) ?? ''));
    if (blank >= 0) { expect(shrunk.lines[blank / 2]![0]).toBe(' '); expect(shrunk.owners[blank / 2]![0]).toBe(-1); }
    // Each box keeps its #k tag on top of the dots where the shrunk box is wide enough to hold it.
    expect(shrunk.lines.join('\n')).toContain(`#${mission.index}`);
    expect(shrinkGraph(layout, 8).lines.join('').replace(/[⠀-⣿ #\d]/gu, '')).toBe('');
  });
  it('hit-tests a canvas cell to the box drawn there', () => {
    const { boxes } = layoutGraph(root); const target = box(boxes, 'mnode_impl');
    expect(boxAt(boxes, target.left + 1, target.top + 1)).toBe(boxes.indexOf(target));
    expect(boxAt(boxes, target.right, target.bottom)).toBe(boxes.indexOf(target));
    expect(boxAt(boxes, -5, -5)).toBe(-1);
  });
  it('is empty without a root', () => {
    expect(layoutGraph(undefined)).toEqual({ lines: [], boxes: [] });
  });
});
