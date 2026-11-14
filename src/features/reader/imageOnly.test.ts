import { describe, expect, it } from 'vitest';
import { isImageOnly } from './ReflowView';

describe('isImageOnly', () => {
  it('is true for image-only blocks', () => {
    expect(isImageOnly({ content: [{ t: 'image_ref', resourceId: 'a', alt: 'x' }], sourceText: '' })).toBe(true);
    expect(isImageOnly({ content: [{ t: 'text', v: ' ' }, { t: 'image_ref', resourceId: 'a', alt: '' }, { t: 'break' }], sourceText: '' })).toBe(true);
  });
  it('is false when there is text to translate or no image', () => {
    expect(isImageOnly({ content: [{ t: 'image_ref', resourceId: 'a', alt: '' }, { t: 'text', v: 'Figure 1' }], sourceText: 'Figure 1' })).toBe(false);
    expect(isImageOnly({ content: [{ t: 'text', v: 'Hello' }], sourceText: 'Hello' })).toBe(false);
    expect(isImageOnly({ content: [], sourceText: '' })).toBe(false);
  });
});
