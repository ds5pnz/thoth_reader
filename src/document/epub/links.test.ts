import { describe, expect, it } from 'vitest';
import { armLinks, parseSpineLink, takeClickedLink } from './EpubOriginalView';

function doc(html: string): Document {
  return new DOMParser().parseFromString(`<html><body>${html}</body></html>`, 'text/html');
}

describe('EPUB links in the script-less frame', () => {
  it('arms each link once and keeps the real target', () => {
    const d = doc('<a id="a" href="3.xhtml#n1" target="_blank">note</a><a id="b" href="https://example.com/" data-thoth-external="1">site</a><a name="x">anchor</a>');
    expect(armLinks(d)).toBe(2);
    expect(armLinks(d)).toBe(0);
    const a = d.getElementById('a')!;
    expect(a.getAttribute('href')).toMatch(/^#thoth-l-\d+$/);
    expect(a.getAttribute('data-thoth-href')).toBe('3.xhtml#n1');
    expect(a.hasAttribute('target')).toBe(false);
    expect(d.getElementById('b')!.getAttribute('href')).not.toBe(a.getAttribute('href'));
  });

  it('finds the clicked link by the fragment and re-arms it', () => {
    const d = doc('<a id="a" href="3.xhtml#n1">note</a><a id="b" href="https://example.com/" data-thoth-external="1">site</a>');
    armLinks(d);
    const b = d.getElementById('b')!;
    const hash = b.getAttribute('href')!;
    expect(takeClickedLink(d, hash)).toEqual({ href: 'https://example.com/', external: true });
    expect(b.getAttribute('href')).not.toBe(hash);
    expect(takeClickedLink(d, hash)).toBeNull(); // the old fragment is spent
    expect(takeClickedLink(d, b.getAttribute('href')!)).toEqual({ href: 'https://example.com/', external: true });
    expect(takeClickedLink(d, '#chapter-heading')).toBeNull();
    expect(takeClickedLink(d, '')).toBeNull();
  });

  it('reads spine links', () => {
    expect(parseSpineLink('3.xhtml#n1')).toEqual({ index: 3, fragment: 'n1' });
    expect(parseSpineLink('thoth-doc://localhost/tok/epub/x/12.xhtml')).toEqual({ index: 12, fragment: null });
    expect(parseSpineLink('https://example.com/')).toBeNull();
    expect(parseSpineLink('notes3.xhtml')).toBeNull();
  });
});
