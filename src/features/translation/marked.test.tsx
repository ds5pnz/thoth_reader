import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { renderMarked, stripInlineTags } from './marked';

describe('renderMarked', () => {
  it('wraps the marked ranges and keeps the text intact', () => {
    const { container } = render(<p>{renderMarked('메모리: 16GB가 제공됩니다.', [{ startU16: 0, endU16: 3, kind: 'b' }])}</p>);
    expect(container.textContent).toBe('메모리: 16GB가 제공됩니다.');
    expect(container.querySelector('strong')?.textContent).toBe('메모리');
    expect(container.querySelectorAll('em').length).toBe(0);
  });
  it('supports nested bold + italic and ignores invalid ranges', () => {
    const { container } = render(<p>{renderMarked('a b c d', [{ startU16: 2, endU16: 5, kind: 'b' }, { startU16: 4, endU16: 5, kind: 'i' }, { startU16: 3, endU16: 99, kind: 'b' }])}</p>);
    expect([...container.querySelectorAll('strong')].map((x) => x.textContent).join('')).toBe('b c');
    expect(container.querySelector('em')?.textContent).toBe('c');
    expect(container.textContent).toBe('a b c d');
  });
  it('returns plain text without marks and strips preview tags', () => {
    expect(renderMarked('plain', [])).toBe('plain');
    expect(stripInlineTags('<b>굵게</b> 그리고 <i>기울임</i>')).toBe('굵게 그리고 기울임');
  });
});
