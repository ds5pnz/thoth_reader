import { describe, expect, it } from 'vitest';
import { styledContent } from './extract';

const identity = (n: number) => [{ sourceStart: 0, sourceEnd: n, rawStart: 0, rawEnd: n }];
const textOf = (nodes: ReturnType<typeof styledContent>): string => nodes.map((n) => ('v' in n ? n.v : 'c' in n ? textOf(n.c) : '')).join('');

describe('styledContent', () => {
  it('marks a leading bold term and keeps the text identical', () => {
    const text = 'Memory: 16 GB is provided.';
    const nodes = styledContent(text, identity(text.length), [{ rawStart: 0, rawEnd: 6, s: 1 }], 'paragraph', false);
    expect(nodes[0]).toEqual({ t: 'strong', c: [{ t: 'text', v: 'Memory' }] });
    expect(textOf(nodes)).toBe(text);
  });
  it('keeps bold open across the space between two bold fragments', () => {
    const text = 'Radio module: dual band';
    const nodes = styledContent(text, identity(text.length), [{ rawStart: 0, rawEnd: 5, s: 1 }, { rawStart: 6, rawEnd: 12, s: 1 }], 'paragraph', false);
    expect(nodes[0]).toEqual({ t: 'strong', c: [{ t: 'text', v: 'Radio module' }] });
    expect(textOf(nodes)).toBe(text);
  });
  it('treats uniform styling by context', () => {
    const text = 'All bold line';
    const all = [{ rawStart: 0, rawEnd: text.length, s: 1 }];
    expect(styledContent(text, identity(text.length), all, 'heading', false)).toEqual([{ t: 'text', v: text }]);
    expect(styledContent(text, identity(text.length), all, 'paragraph', true)).toEqual([{ t: 'text', v: text }]);
    expect(styledContent(text, identity(text.length), all, 'list_item', false)).toEqual([{ t: 'strong', c: [{ t: 'text', v: text }] }]);
    expect(styledContent(text, identity(text.length), [], 'paragraph', false)).toEqual([{ t: 'text', v: text }]);
  });
  it('nests italic inside bold', () => {
    const text = 'a b c';
    const nodes = styledContent(text, identity(5), [{ rawStart: 2, rawEnd: 3, s: 1 }, { rawStart: 4, rawEnd: 5, s: 3 }], 'paragraph', false);
    expect(textOf(nodes)).toBe(text);
    expect(nodes.at(-1)).toEqual({ t: 'strong', c: [{ t: 'emphasis', c: [{ t: 'text', v: 'c' }] }] });
  });
});
