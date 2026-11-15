import { useEffect, useState } from 'react';
import { call } from '../../ipc/client';
import type { ActiveProfile } from '../../ipc/generated/ActiveProfile';

const CHANGED = 'thoth:active-profile-changed';
/** Tell every view that the provider or profile behind translations changed (rename, switch). */
export function notifyActiveProfileChanged() {
  window.dispatchEvent(new Event(CHANGED));
}

const KIND_LABEL: Record<string, string> = { ollama: 'Ollama', openai_compatible: 'OpenAI 호환', chatgpt: 'ChatGPT 요금제', codex: 'Codex' };
const MODE_LABEL: Record<string, string> = { structured: '표준', simple: '단순', rosetta: 'Rosetta' };
export const providerKindLabel = (kind: string) => KIND_LABEL[kind] ?? kind;
export const promptModeLabel = (mode: string) => MODE_LABEL[mode] ?? mode;
/** Name to show for a provider: its label, or its kind when the label is blank. */
export const providerName = (label: string | null | undefined, kind: string) => (label ?? '').trim() || providerKindLabel(kind);

/** One line saying what translates: "LM Studio · hy-mt2-7b · 단순". */
export function describeActiveProfile(p: ActiveProfile): string {
  return `${providerName(p.providerLabel, p.providerKind)} · ${p.modelId} · ${promptModeLabel(p.promptMode)}`;
}

/** The profile translations use right now; `null` when none is set. Refetches when it changes. */
export function useActiveProfile(activeProfileId: string | null | undefined): ActiveProfile | null {
  const [profile, setProfile] = useState<ActiveProfile | null>(null);
  useEffect(() => {
    let live = true;
    const load = () => {
      if (!activeProfileId) { setProfile(null); return; }
      void call<ActiveProfile | null>('profile_active').then((p) => { if (live) setProfile(p); }).catch(() => { if (live) setProfile(null); });
    };
    load();
    window.addEventListener(CHANGED, load);
    return () => { live = false; window.removeEventListener(CHANGED, load); };
  }, [activeProfileId]);
  return profile;
}
