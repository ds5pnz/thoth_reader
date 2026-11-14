import { useEffect, useRef } from 'react';

/** True while focus is in an editable field: reader shortcuts must not steal input. */
export function isEditing(el: Element | null): boolean {
  if (!el) return false;
  const h = el as HTMLElement;
  return h.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(h.tagName);
}

/** combo: "mod+o", "mod+shift+f", "escape". mod = Cmd on macOS. */
export function matches(e: KeyboardEvent, combo: string): boolean {
  const parts = combo.toLowerCase().split('+');
  const key = parts.pop()!;
  const mod = parts.includes('mod');
  const shift = parts.includes('shift');
  const alt = parts.includes('alt');
  if (mod !== (e.metaKey || e.ctrlKey)) return false;
  if (shift !== e.shiftKey || alt !== e.altKey) return false;
  if (e.key.toLowerCase() === key) return true;
  // Physical-key fallback: with a Korean input source Cmd+D arrives as 'ㅇ', '\\' as '₩'.
  const code = key.length === 1 && key >= 'a' && key <= 'z' ? `Key${key.toUpperCase()}` : key === '\\' ? 'Backslash' : key === ',' ? 'Comma' : null;
  return code !== null && e.code === code;
}

export function useShortcut(combo: string, handler: (e: KeyboardEvent) => void, opts: { allowInInputs?: boolean; enabled?: boolean; allowInModal?: boolean } = {}) {
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => {
    if (opts.enabled === false) return;
    const onKey = (e: KeyboardEvent) => {
      if (!matches(e, combo)) return;
      // A modal dialog owns the keyboard: nothing behind it reacts.
      if (!opts.allowInModal && document.querySelector('[aria-modal="true"]')) return;
      if (!opts.allowInInputs && isEditing(document.activeElement) && !combo.startsWith('mod+')) return;
      // Mod-shortcuts other than save also yield to an active editor.
      if (!opts.allowInInputs && isEditing(document.activeElement) && combo !== 'mod+,' ) return;
      e.preventDefault();
      ref.current(e);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [combo, opts.allowInInputs, opts.enabled, opts.allowInModal]);
}
