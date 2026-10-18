import { describe, expect, it } from 'vitest';
import { layoutGraph, moveSelection, suggestedCommand, type GraphBox } from './map-graph.js';

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
  it('is empty without a root', () => {
    expect(layoutGraph(undefined)).toEqual({ lines: [], boxes: [] });
  });
});
