import { describe, expect, it } from 'vitest';
import { applyTheme } from './EpubOriginalView';

describe('book theme', () => {
  it('turns dark on and off without touching the book content', () => {
    const d = new DOMParser().parseFromString('<html><head><style>p{color:#000}</style></head><body><p>text</p></body></html>', 'text/html');
    applyTheme(d, false);
    expect(d.documentElement.classList.contains('thoth-dark')).toBe(false);
    expect(d.getElementById('thoth-dark-style')).toBeNull();
    applyTheme(d, true);
    applyTheme(d, true);
    expect(d.documentElement.classList.contains('thoth-dark')).toBe(true);
    expect(d.querySelectorAll('#thoth-dark-style').length).toBe(1);
    expect(d.querySelectorAll('style').length).toBe(2);
    applyTheme(d, false);
    expect(d.documentElement.classList.contains('thoth-dark')).toBe(false);
    expect(d.body.innerHTML).toBe('<p>text</p>');
  });
});
