import { describe, expect, it } from 'vitest';
import { isPreserved, pictureNodes } from './ReflowView';

describe('reflow rows', () => {
  it('finds the pictures of a block, nested ones too, and leaves formulas alone', () => {
    const nodes = [
      { t: 'image_ref' as const, resourceId: 'a', alt: '' },
      { t: 'text' as const, v: 'Figure 1-1. Caption' },
      { t: 'link' as const, href: 'https://example.com/', c: [{ t: 'image_ref' as const, resourceId: 'b', alt: '' }] },
      { t: 'math_ref' as const, resourceId: 'm', alt: 'x' },
    ];
    expect(pictureNodes(nodes).map((n) => (n.t === 'image_ref' ? n.resourceId : ''))).toEqual(['a', 'b']);
  });
  it('treats a figure with a caption as translatable and code as preserved', () => {
    expect(isPreserved({ kind: 'figure', translationPolicy: 'preserve', sourceText: 'Figure 1-1. How it works' }, undefined)).toBe(false);
    expect(isPreserved({ kind: 'figure', translationPolicy: 'preserve', sourceText: '' }, undefined)).toBe(true);
    expect(isPreserved({ kind: 'code', translationPolicy: 'preserve', sourceText: 'let x = 1' }, undefined)).toBe(true);
    expect(isPreserved({ kind: 'paragraph', translationPolicy: 'auto', sourceText: 'Text' }, undefined)).toBe(false);
    // once the backend has spoken, its status decides
    expect(isPreserved({ kind: 'paragraph', translationPolicy: 'auto', sourceText: 'Text' }, 'excluded')).toBe(true);
    expect(isPreserved({ kind: 'figure', translationPolicy: 'preserve', sourceText: 'Caption' }, 'none')).toBe(false);
  });
});
