import { describe, expect, it, vi } from 'vitest';
import type { CliRenderer, SyntaxStyle } from '@opentui/core';
import type { TranscriptEntry } from './controller.js';

// Mirror OpenTUI's anchor-based add semantics. A plain array assignment mock
// would hide the self-insertion bug that flooded the live terminal with warnings.
vi.mock('@opentui/core', () => {
  class Renderable {
    id: string;
    children: Renderable[] = [];
    isDestroyed = false;
    content = '';
    streaming = false;
    constructor(_renderer: unknown, options: { id: string }) { this.id = options.id; }
    add(child: Renderable, index?: number) {
      const anchor = index === undefined ? undefined : this.children[index];
      if (anchor === child) throw new Error('Cannot insert a node before itself');
      const previous = this.children.indexOf(child);
      if (previous !== -1) this.children.splice(previous, 1);
      this.children.splice(anchor ? this.children.indexOf(anchor) : this.children.length, 0, child);
    }
    remove(child: Renderable) { this.children.splice(this.children.indexOf(child), 1); }
    getChildren() { return [...this.children]; }
    destroyRecursively() { this.isDestroyed = true; this.children.forEach(child => child.destroyRecursively()); }
  }
  return { BoxRenderable: Renderable, TextRenderable: Renderable,
    MarkdownRenderable: class extends Renderable {}, TextAttributes: { BOLD: 1 }, SyntaxStyle: {} };
});
import { createTranscript } from './visual.js';

const entry = (id: string, text = id): TranscriptEntry => ({ id, kind: 'assistant', title: 'Assistant', text, sequence: null, status: 'streaming' });
const setup = () => createTranscript({} as CliRenderer, {} as SyntaxStyle);
const ids = (transcript: ReturnType<typeof setup>) => transcript.root.getChildren().map(child => child.id);

describe('transcript reconciliation', () => {
  it('does not reinsert correct children on initial render, append, or replay', () => {
    const transcript = setup();
    const add = vi.spyOn(transcript.root, 'add');
    transcript.update([entry('a'), entry('b')]);
    expect(add).toHaveBeenCalledTimes(2);
    const original = transcript.root.getChildren();
    add.mockClear();
    transcript.update([entry('a'), entry('b'), entry('c')]);
    expect(add).toHaveBeenCalledTimes(1);
    transcript.update([entry('a'), entry('b'), entry('c')]);
    expect(add).toHaveBeenCalledTimes(1);
    expect(transcript.root.getChildren().slice(0, 2)).toEqual(original);
    expect(ids(transcript)).toEqual(['message-a', 'message-b', 'message-c']);
  });

  it('updates streaming text in place and reorders only misplaced nodes', () => {
    const transcript = setup();
    transcript.update([entry('a'), entry('b'), entry('c')]);
    const [a, b, c] = transcript.root.getChildren();
    const body = b!.getChildren()[1]!;
    const add = vi.spyOn(transcript.root, 'add');
    transcript.update([entry('a'), entry('b', 'stream delta'), entry('c')]);
    expect(add).not.toHaveBeenCalled();
    expect(b!.getChildren()[1]).toBe(body);
    expect(body).toMatchObject({ content: 'stream delta', streaming: true });
    transcript.update([entry('c'), entry('a'), { ...entry('b', 'complete'), status: 'complete' }]);
    expect(add).toHaveBeenCalledTimes(1);
    expect(transcript.root.getChildren()).toEqual([c, a, b]);
    expect(body).toMatchObject({ content: 'complete', streaming: false });
    transcript.update([entry('b'), entry('c'), entry('a')]);
    expect(transcript.root.getChildren()).toEqual([b, c, a]);
  });

  it('settles ephemeral messages and inserts replay entries without rebuilding survivors', () => {
    const transcript = setup();
    transcript.update([entry('a'), entry('live')]);
    const [a, live] = transcript.root.getChildren();
    transcript.update([entry('earlier'), entry('a'), entry('persisted')]);
    expect(ids(transcript)).toEqual(['message-earlier', 'message-a', 'message-persisted']);
    expect(transcript.root.getChildren()[1]).toBe(a);
    expect(a!.isDestroyed).toBe(false);
    expect(live!.isDestroyed).toBe(true);
  });

  it('evicts the bounded replay window and clears without self-insertion', () => {
    const transcript = setup();
    const entries = Array.from({ length: 100 }, (_, i) => entry(String(i)));
    transcript.update(entries);
    const [first, second] = transcript.root.getChildren();
    transcript.update([...entries, entry('100')]);
    expect(transcript.root.getChildren()).toHaveLength(100);
    expect(transcript.root.getChildren()[0]).toBe(second);
    expect(first!.isDestroyed).toBe(true);
    expect(ids(transcript).at(-1)).toBe('message-100');
    transcript.update([]);
    expect(ids(transcript)).toEqual([]);
    expect(second!.isDestroyed).toBe(true);
    transcript.update([entry('new-project')]);
    expect(ids(transcript)).toEqual(['message-new-project']);
  });
});
