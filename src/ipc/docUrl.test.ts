import { describe, expect, it } from 'vitest';
import { docOrigin, docUrl, usesHttpProtocolForm } from './docUrl';

const MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)';
const WIN = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0';
const ANDROID = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36';
const LINUX = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko)';

describe('document protocol addresses', () => {
  it('uses the scheme form on macOS and Linux, the http form on Windows and Android', () => {
    expect(docOrigin(MAC)).toBe('thoth-doc://localhost');
    expect(docOrigin(LINUX)).toBe('thoth-doc://localhost');
    expect(docOrigin(WIN)).toBe('http://thoth-doc.localhost');
    expect(docOrigin(ANDROID)).toBe('http://thoth-doc.localhost');
    expect(usesHttpProtocolForm('')).toBe(false);
  });
  it('joins a path without doubling the slash', () => {
    expect(docUrl('tok/pdf')).toBe(`${docOrigin()}/tok/pdf`);
    expect(docUrl('/tok/epub/')).toBe(`${docOrigin()}/tok/epub/`);
  });
});
