// BCP 47 tag -> flag emoji for the toolbar. The region subtag wins; otherwise a
// conventional default region per language. Unknown languages fall back to the tag text.
const DEFAULT_REGION: Record<string, string> = {
  en: 'US', ko: 'KR', ja: 'JP', zh: 'CN', de: 'DE', fr: 'FR', es: 'ES', it: 'IT', pt: 'PT', ru: 'RU',
  nl: 'NL', sv: 'SE', da: 'DK', nb: 'NO', no: 'NO', fi: 'FI', pl: 'PL', cs: 'CZ', tr: 'TR', vi: 'VN',
  id: 'ID', th: 'TH', hi: 'IN', ar: 'SA', he: 'IL', uk: 'UA', el: 'GR', hu: 'HU', ro: 'RO',
};

export function regionOf(tag: string): string | null {
  const parts = tag.trim().split(/[-_]/);
  const region = parts.slice(1).find((p) => /^[A-Za-z]{2}$/.test(p));
  if (region) return region.toUpperCase();
  return DEFAULT_REGION[(parts[0] ?? '').toLowerCase()] ?? null;
}

export function flagOf(tag: string): string | null {
  const r = regionOf(tag);
  if (!r) return null;
  return String.fromCodePoint(...[...r].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65));
}
