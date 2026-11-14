import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { call, mutate } from '../ipc/client';

export type SettingsMap = {
  theme?: 'system' | 'light' | 'dark';
  fontSize?: number;
  fontFamily?: string;
  lineHeight?: number;
  defaultTargetLanguage?: string;
  defaultSourceLanguage?: string;
  activeProfileId?: string | null;
  autoTranslate?: boolean;
  syncScroll?: boolean;
  glossary?: { source: string; target: string }[];
  promptTemplate?: string | null;
  readingMode?: 'parallel' | 'interleaved' | 'original';
  pdfView?: 'original' | 'reflow';
  privacyAcknowledged?: boolean;
};

type Versioned = Record<string, { value: unknown; version: number }>;

interface Ctx {
  settings: SettingsMap;
  loaded: boolean;
  set: <K extends keyof SettingsMap>(key: K, value: SettingsMap[K]) => Promise<void>;
  reload: () => Promise<void>;
}

const SettingsContext = createContext<Ctx | null>(null);

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [raw, setRaw] = useState<Versioned>({});
  const [loaded, setLoaded] = useState(false);
  const reload = useCallback(async () => {
    try {
      setRaw(await call<Versioned>('settings_get'));
    } finally {
      setLoaded(true);
    }
  }, []);
  useEffect(() => { void reload(); }, [reload]);
  const settings = useMemo(() => {
    const s: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(raw)) s[k] = v.value;
    return s as SettingsMap;
  }, [raw]);
  const set = useCallback(async <K extends keyof SettingsMap>(key: K, value: SettingsMap[K]) => {
    const expectedVersion = raw[key]?.version ?? null;
    const r = await mutate<{ version: number }>('settings_patch', { key, value, expectedVersion });
    setRaw((prev) => ({ ...prev, [key]: { value, version: r.version } }));
  }, [raw]);

  // Theme + reading typography are applied to :root so every view follows them.
  useEffect(() => {
    const root = document.documentElement;
    const t = settings.theme ?? 'system';
    if (t === 'system') root.removeAttribute('data-theme');
    else root.setAttribute('data-theme', t);
    root.style.setProperty('--reader-size', `${settings.fontSize ?? 17}px`);
    root.style.setProperty('--reader-line', String(settings.lineHeight ?? 1.65));
    if (settings.fontFamily) root.style.setProperty('--reader-font', settings.fontFamily);
    else root.style.removeProperty('--reader-font');
  }, [settings.theme, settings.fontSize, settings.lineHeight, settings.fontFamily]);

  return <SettingsContext.Provider value={{ settings, loaded, set, reload }}>{children}</SettingsContext.Provider>;
}

export function useSettings(): Ctx {
  const c = useContext(SettingsContext);
  if (!c) throw new Error('SettingsProvider missing');
  return c;
}
