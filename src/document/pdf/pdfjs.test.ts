import { describe, expect, it } from 'vitest';
import { buildDocumentParams, makeBundledBinaryDataFactory, safeLinkUrl } from './pdfjs';

const assets = {
  workerSrc: '/assets/pdf.worker-abc.mjs',
  resolve: (kind: string, filename: string) => (kind === 'cMapUrl' && filename === 'UniJIS-UCS2-H.bcmap' ? '/assets/UniJIS-UCS2-H-123.bcmap' : undefined),
};

describe('buildDocumentParams', () => {
  it('produces the hardened configuration', () => {
    const p = buildDocumentParams('thoth-doc://localhost/s/d', assets) as Record<string, unknown>;
    expect(p).toMatchObject({
      url: 'thoth-doc://localhost/s/d',
      disableAutoFetch: true,
      disableStream: false,
      disableRange: false,
      rangeChunkSize: 65536,
      cMapPacked: true,
      useWorkerFetch: false,
      useSystemFonts: false,
      isEvalSupported: false,
      enableXfa: false,
      stopAtErrors: false,
      withCredentials: false,
      fontExtraProperties: true,
    });
    expect(p.enableScripting).toBeUndefined();
    expect(p.password).toBeUndefined();
    for (const k of ['cMapUrl', 'standardFontDataUrl', 'wasmUrl']) {
      expect(String(p[k])).toMatch(/^thoth-bundled:\/.*\/$/); // never http(s): nothing remote
    }
    expect(p.iccUrl).toBeUndefined();
    expect(typeof p.BinaryDataFactory).toBe('function');
  });
});

describe('BundledBinaryDataFactory', () => {
  it('only serves bundled files and validates names', async () => {
    const F = makeBundledBinaryDataFactory(assets);
    const f = new F({});
    await expect(f.fetch({ kind: 'cMapUrl', filename: 'Missing.bcmap' })).rejects.toThrow(/not bundled/);
    await expect(f.fetch({ kind: 'cMapUrl', filename: '../../etc/passwd' })).rejects.toThrow(/invalid/);
    await expect(f.fetch({ kind: 'iccUrl', filename: 'x.icc' })).rejects.toThrow(/unsupported/);
  });
});

describe('safeLinkUrl', () => {
  it('accepts only absolute http(s) URLs without credentials', () => {
    expect(safeLinkUrl('https://example.org/a?b=1')).toBe('https://example.org/a?b=1');
    expect(safeLinkUrl('http://example.org')).toBe('http://example.org/');
    expect(safeLinkUrl('javascript:alert(1)')).toBeNull();
    expect(safeLinkUrl('file:///etc/passwd')).toBeNull();
    expect(safeLinkUrl('mailto:a@b.c')).toBeNull();
    expect(safeLinkUrl('https://user:pw@example.org')).toBeNull();
    expect(safeLinkUrl('/relative')).toBeNull();
    expect(safeLinkUrl(42)).toBeNull();
  });
});

describe('bundled pdf.js assets (Vite ?url&no-inline globs)', () => {
  it('resolves every CMap, standard font and decoder wasm locally', async () => {
    const m = await import('./pdfjsAssets');
    expect(m.bundledAssetCounts()).toEqual({ cMapUrl: 168, standardFontDataUrl: 14, wasmUrl: 2 });
    expect(m.bundledAssetUrl('cMapUrl', 'UniJIS-UCS2-H.bcmap')).toBeTruthy();
    expect(m.bundledAssetUrl('standardFontDataUrl', 'FoxitSerif.pfb')).toBeTruthy();
    expect(m.bundledAssetUrl('wasmUrl', 'openjpeg.wasm')).toBeTruthy();
    expect(m.bundledAssetUrl('wasmUrl', 'quickjs-eval.wasm')).toBeUndefined();
    expect(m.PDFJS_WORKER_URL).toMatch(/pdf\.worker\.mjs/);
    for (const u of [m.PDFJS_WORKER_URL, m.bundledAssetUrl('cMapUrl', 'UniJIS-UCS2-H.bcmap')!]) expect(u).not.toMatch(/^(https?:|data:)/);
  });
});
