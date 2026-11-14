// Renders a stored translation with its bold/italic ranges (UTF-16 offsets from the backend).
// Plain React elements only — never HTML.
import type { ReactNode } from 'react';
import type { Mark } from '../../ipc/generated/Mark';

/** Split `text` at mark boundaries and wrap each piece in <strong>/<em> as needed. */
export function renderMarked(text: string, marks: readonly Mark[] | undefined): ReactNode {
  if (!marks || marks.length === 0) return text;
  const valid = marks.filter((m) => m.startU16 >= 0 && m.endU16 <= text.length && m.startU16 < m.endU16);
  if (valid.length === 0) return text;
  const cuts = [...new Set([0, text.length, ...valid.flatMap((m) => [m.startU16, m.endU16])])].sort((a, b) => a - b);
  const out: ReactNode[] = [];
  for (let i = 0; i + 1 < cuts.length; i++) {
    const a = cuts[i]!;
    const b = cuts[i + 1]!;
    const piece = text.slice(a, b);
    const bold = valid.some((m) => m.kind === 'b' && m.startU16 <= a && m.endU16 >= b);
    const italic = valid.some((m) => m.kind === 'i' && m.startU16 <= a && m.endU16 >= b);
    let node: ReactNode = piece;
    if (italic) node = <em>{node}</em>;
    if (bold) node = <strong>{node}</strong>;
    out.push(bold || italic ? <span key={a}>{node}</span> : piece);
  }
  return out;
}

/** Streaming previews may show raw emphasis tags before the final text is parsed. */
export function stripInlineTags(s: string): string {
  return s.replace(/<\/?[bi]>/gi, '');
}
