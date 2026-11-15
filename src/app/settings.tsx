import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
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
  readingMode?: 'parallel' | 'interleaved' | 'original' | 'translation';
  pdfView?: 'original' | 'reflow';
  privacyAcknowledged?: boolean;
  translationStyle?: 'polite' | 'plain';
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
  const rawRef = useRef<Versioned>({});
  const [loaded, setLoaded] = useState(false);
  const reload = useCallback(async () => {
    try {
      const v = await call<Versioned>('settings_get');
      rawRef.current = v;
      setRaw(v);
    } finally {
      setLoaded(true);
    }
  }, []);
  useEffect(() => { void reload(); }, [reload]);
  useEffect(() => {
    if (import.meta.env.DEV) window.__thoth = { ...(window.__thoth ?? {}), reloadSettings: reload };
  }, [reload]);
  const settings = useMemo(() => {
    const s: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(raw)) s[k] = v.value;
    return s as SettingsMap;
  }, [raw]);
  const set = useCallback(async <K extends keyof SettingsMap>(key: K, value: SettingsMap[K]) => {
    // UI preferences are last-writer-wins: on a version conflict (e.g. rapid toggles),
    // retry once against the server's current version.
    const attempt = (expectedVersion: number | null) => mutate<{ version: number }>('settings_patch', { key, value, expectedVersion });
    let r: { version: number };
    try {
      r = await attempt(rawRef.current[key]?.version ?? null);
    } catch (e) {
      const cur = (e as { error?: { code?: string; details?: { currentVersion?: number } } }).error;
      if (cur?.code !== 'CONFLICT' || typeof cur.details?.currentVersion !== 'number') throw e;
      r = await attempt(cur.details.currentVersion);
    }
    setRaw((prev) => {
      const next = { ...prev, [key]: { value, version: r.version } };
      rawRef.current = next;
      return next;
    });
  }, []);

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

/** Whether the app is showing its dark theme (the setting, or the system when it follows it). */
export function useIsDark(): boolean {
  const { settings } = useSettings();
  const [system, setSystem] = useState(() => typeof matchMedia === 'function' && matchMedia('(prefers-color-scheme: dark)').matches);
  useEffect(() => {
    if (typeof matchMedia !== 'function') return;
    const m = matchMedia('(prefers-color-scheme: dark)');
    const on = () => setSystem(m.matches);
    m.addEventListener('change', on);
    return () => m.removeEventListener('change', on);
  }, []);
  const t = settings.theme ?? 'system';
  return t === 'dark' || (t === 'system' && system);
}
