import { describe, expect, it } from 'vitest';
import { countOccurrences, findQuoteRange, ink, inkOf, inkPosition, nthIndex } from './quote';

function dom(html: string): HTMLElement {
  const d = document.createElement('div');
  d.innerHTML = html;
  document.body.appendChild(d);
  return d;
}

describe('quote anchoring', () => {
  it('finds a quote across element boundaries and whitespace differences', () => {
    const d = dom('<p>The <b>quick  brown</b>\n fox <i>jumps</i> over.</p>');
    const r = findQuoteRange(d, 'quick brown fox', 0)!;
    expect(ink(r.toString())).toBe('quickbrownfox');
    // text layer style: one span per run, no spaces between spans
    const tl = dom('<span>The qu</span><span>ick brown</span><span>fox jumps</span>');
    expect(ink(findQuoteRange(tl, 'quick brown fox', 0)!.toString())).toBe('quickbrownfox');
  });
  it('picks the n-th occurrence and reports missing ones', () => {
    const d = dom('<p>ab ab ab</p>');
    const second = findQuoteRange(d, 'ab', 1)!;
    expect(second.startOffset).toBe(3);
    expect(findQuoteRange(d, 'ab', 3)).toBeNull();
    expect(findQuoteRange(d, 'zz', 0)).toBeNull();
    expect(countOccurrences('ababab', 'ab')).toBe(3);
    expect(countOccurrences('ababab', 'ab', 3)).toBe(2);
    expect(nthIndex('ababab', 'ab', 2)).toBe(4);
  });
  it('Korean text and ink positions', () => {
    const d = dom('<div>분산 시스템은 <em>노드</em>로 이루어집니다. 노드는 서로 통신합니다.</div>');
    const t = inkOf(d);
    expect(t.text.startsWith('분산시스템은노드로')).toBe(true);
    const em = d.querySelector('em')!.firstChild!;
    expect(inkPosition(d, em, 0)).toBe(6);
    expect(findQuoteRange(d, '노드', 1)!.toString()).toBe('노드');
  });
});
