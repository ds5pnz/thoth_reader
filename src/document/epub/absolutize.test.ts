import { describe, expect, it } from 'vitest';
import { absolutizeResourceRefs } from './EpubOriginalView';

describe('absolutizeResourceRefs', () => {
  it('rewrites only package-relative attribute references', () => {
    const base = 'thoth-doc://localhost/tok/epub/';
    const src = '<img src="../r/a.jpg" alt="x"/><link href="../r/s.css"/><a href="3.xhtml#f">../r/not-an-attr</a>';
    expect(absolutizeResourceRefs(src, base)).toBe('<img src="thoth-doc://localhost/tok/epub/r/a.jpg" alt="x"/><link href="thoth-doc://localhost/tok/epub/r/s.css"/><a href="3.xhtml#f">../r/not-an-attr</a>');
  });
});
