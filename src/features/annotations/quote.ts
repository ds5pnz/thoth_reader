// Quote anchoring: an annotation is "these words, the n-th time they occur in this paragraph".
// Matching ignores whitespace so the same quote is found in every rendering of the text
// (reflow rows, the PDF text layer with one span per run, fitted translation overlays).

/** Text with all whitespace removed plus, per kept character, where it came from. */
export interface InkText { text: string; nodes: Text[]; offsets: number[] }

export const ink = (s: string) => s.replace(/\s+/gu, '');

export function inkOf(container: Node): InkText {
  const nodes: Text[] = [];
  const offsets: number[] = [];
  let text = '';
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const t = n as Text;
    const v = t.data;
    for (let i = 0; i < v.length; i++) {
      const ch = v[i]!;
      if (/\s/u.test(ch)) continue;
      text += ch;
      nodes.push(t);
      offsets.push(i);
    }
  }
  return { text, nodes, offsets };
}

export function countOccurrences(hay: string, needle: string, before = Infinity): number {
  if (!needle) return 0;
  let n = 0;
  for (let i = hay.indexOf(needle); i >= 0 && i < before; i = hay.indexOf(needle, i + 1)) n++;
  return n;
}

/** Start index (in ink characters) of the `occurrence`-th match, or -1. */
export function nthIndex(hay: string, needle: string, occurrence: number): number {
  if (!needle) return -1;
  let i = hay.indexOf(needle);
  for (let k = 0; k < occurrence && i >= 0; k++) i = hay.indexOf(needle, i + 1);
  return i;
}

export function rangeAt(t: InkText, start: number, length: number): Range | null {
  const end = start + length - 1;
  if (start < 0 || length <= 0 || end >= t.nodes.length) return null;
  const r = document.createRange();
  r.setStart(t.nodes[start]!, t.offsets[start]!);
  r.setEnd(t.nodes[end]!, t.offsets[end]! + 1);
  return r;
}

export function findQuoteRange(container: Node, quote: string, occurrence: number, skipOccurrences = 0): Range | null {
  const t = inkOf(container);
  const q = ink(quote);
  return rangeAt(t, nthIndex(t.text, q, occurrence + skipOccurrences), q.length);
}

/** Ink position of a DOM point inside `container`. */
export function inkPosition(container: Node, node: Node, offset: number): number {
  const r = document.createRange();
  r.selectNodeContents(container);
  try {
    r.setEnd(node, offset);
  } catch {
    return 0;
  }
  return ink(r.toString()).length;
}

/** Every match of `query` in `container` (case- and whitespace-insensitive), capped. */
export function findAll(container: Node, query: string, cap = 200): Range[] {
  const q = ink(query).toLocaleLowerCase();
  if (!q) return [];
  const t = inkOf(container);
  const hay = t.text.toLocaleLowerCase();
  const out: Range[] = [];
  // toLocaleLowerCase can change length for a few characters; skip such containers rather than mis-anchor
  if (hay.length !== t.text.length) return out;
  for (let i = hay.indexOf(q); i >= 0 && out.length < cap; i = hay.indexOf(q, i + q.length)) {
    const r = rangeAt(t, i, q.length);
    if (r) out.push(r);
  }
  return out;
}

/** Like [`inkOf`], limited to the part of the document inside `range` (any document). */
export function inkOfRange(range: Range): InkText {
  const nodes: Text[] = [];
  const offsets: number[] = [];
  let text = '';
  const root = range.commonAncestorContainer;
  const doc = root.ownerDocument ?? (root as Document);
  const take = (t: Text) => {
    const from = t === range.startContainer ? range.startOffset : 0;
    const to = t === range.endContainer ? range.endOffset : t.data.length;
    for (let i = from; i < to; i++) {
      const ch = t.data[i]!;
      if (/\s/u.test(ch)) continue;
      text += ch;
      nodes.push(t);
      offsets.push(i);
    }
  };
  if (root.nodeType === 3) {
    take(root as Text);
    return { text, nodes, offsets };
  }
  const walker = doc.createTreeWalker(root, 4 /* NodeFilter.SHOW_TEXT */);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (range.intersectsNode(n)) take(n as Text);
  }
  return { text, nodes, offsets };
}

function rangeIn(doc: Document, t: InkText, start: number, length: number): Range | null {
  const end = start + length - 1;
  if (start < 0 || length <= 0 || end >= t.nodes.length) return null;
  const r = doc.createRange();
  r.setStart(t.nodes[start]!, t.offsets[start]!);
  r.setEnd(t.nodes[end]!, t.offsets[end]! + 1);
  return r;
}

export function findQuoteInRange(range: Range, quote: string, occurrence: number): Range | null {
  const t = inkOfRange(range);
  const q = ink(quote);
  const doc = range.startContainer.ownerDocument;
  return doc ? rangeIn(doc, t, nthIndex(t.text, q, occurrence), q.length) : null;
}

/** Ink characters of `range` that come before the point (node, offset). */
export function inkBefore(range: Range, node: Node, offset: number): number {
  const r = range.cloneRange();
  try {
    r.setEnd(node, offset);
  } catch {
    return 0;
  }
  return inkOfRange(r).text.length;
}
