import { useEffect, useRef, type HTMLAttributes } from 'react';

const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * Modal backdrop that owns the keyboard while it is open: focus moves into the dialog,
 * Tab stays inside it, Escape closes it wherever focus was, and focus returns on close.
 */
export function ModalBackdrop({ onClose, children, ...rest }: { onClose?: () => void } & HTMLAttributes<HTMLDivElement>) {
  const ref = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const root = ref.current;
    if (!root) return;
    const before = document.activeElement as HTMLElement | null;
    if (!root.contains(document.activeElement)) {
      const first = root.querySelector<HTMLElement>('[autofocus]') ?? root.querySelector<HTMLElement>(FOCUSABLE);
      (first ?? root).focus({ preventScroll: true });
    }
    const onKey = (e: KeyboardEvent) => {
      // Only the topmost dialog reacts.
      const all = document.querySelectorAll('.modal-backdrop');
      if (all[all.length - 1] !== root) return;
      if (e.key === 'Escape' && close.current) {
        e.preventDefault();
        e.stopPropagation();
        close.current();
        return;
      }
      if (e.key !== 'Tab') return;
      const items = [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null);
      if (items.length === 0) { e.preventDefault(); root.focus(); return; }
      const first = items[0]!;
      const last = items[items.length - 1]!;
      const active = document.activeElement;
      if (!root.contains(active)) { e.preventDefault(); first.focus(); }
      else if (e.shiftKey && active === first) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && active === last) { e.preventDefault(); first.focus(); }
    };
    window.addEventListener('keydown', onKey, true);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      if (before && document.contains(before)) before.focus({ preventScroll: true });
    };
  }, []);
  return <div ref={ref} className="modal-backdrop" aria-modal="true" tabIndex={-1} {...rest}>{children}</div>;
}
