#!/usr/bin/env node
// Generates small, hand-written, redistributable PDF fixtures for the PDF
// extraction tests (spec §10.2 T01–T04) plus golden `*.expected.json` files.
//
//   node scripts/fixtures/make-pdf-fixtures.mjs
//
// No dependencies: PDFs are written by hand (uncompressed content streams,
// standard Type1 fonts, a correct xref table). Output is deterministic, so the
// committed fixtures can be regenerated and diffed.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, '..', '..', 'tests', 'fixtures', 'pdf');

// ---------------------------------------------------------------------------
// Minimal PDF writer
// ---------------------------------------------------------------------------

class PdfWriter {
  constructor() {
    /** @type {(Buffer|null)[]} */
    this.objects = [];
  }
  alloc() {
    this.objects.push(null);
    return this.objects.length;
  }
  set(id, body) {
    this.objects[id - 1] = Buffer.isBuffer(body) ? body : Buffer.from(body, 'latin1');
  }
  add(body) {
    const id = this.alloc();
    this.set(id, body);
    return id;
  }
  stream(dict, data) {
    const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data, 'latin1');
    return Buffer.concat([
      Buffer.from(`<< ${dict} /Length ${bytes.length} >>\nstream\n`, 'latin1'),
      bytes,
      Buffer.from('\nendstream', 'latin1'),
    ]);
  }
  build(rootId) {
    const parts = [Buffer.from('%PDF-1.7\n%\xE2\xE3\xCF\xD3\n', 'latin1')];
    let offset = parts[0].length;
    const offsets = [];
    this.objects.forEach((body, i) => {
      if (!body) throw new Error(`object ${i + 1} never set`);
      offsets.push(offset);
      const chunk = Buffer.concat([
        Buffer.from(`${i + 1} 0 obj\n`, 'latin1'),
        body,
        Buffer.from('\nendobj\n', 'latin1'),
      ]);
      parts.push(chunk);
      offset += chunk.length;
    });
    const xrefStart = offset;
    let xref = `xref\n0 ${this.objects.length + 1}\n0000000000 65535 f \n`;
    for (const o of offsets) xref += `${String(o).padStart(10, '0')} 00000 n \n`;
    xref += `trailer\n<< /Size ${this.objects.length + 1} /Root ${rootId} 0 R >>\nstartxref\n${xrefStart}\n%%EOF\n`;
    parts.push(Buffer.from(xref, 'latin1'));
    return Buffer.concat(parts);
  }
}

const STD_FONTS = {
  F1: 'Helvetica',
  F2: 'Helvetica-Bold',
  F3: 'Times-Roman',
  F4: 'Times-Italic',
  F5: 'Courier',
  F6: 'Symbol',
  F7: 'Times-Bold',
};

/** Escape a JS string (WinAnsi subset) for a PDF literal string. */
function pdfString(s) {
  let out = '';
  for (const ch of s) {
    const code = ch.codePointAt(0);
    const mapped = WINANSI_EXTRA[ch] ?? code;
    if (mapped > 255) throw new Error(`char not encodable in WinAnsi: ${ch}`);
    const c = String.fromCharCode(mapped);
    if (c === '(' || c === ')' || c === '\\') out += '\\' + c;
    else if (mapped < 32 || mapped > 126) out += '\\' + mapped.toString(8).padStart(3, '0');
    else out += c;
  }
  return `(${out})`;
}
const WINANSI_EXTRA = { '•': 0x95, '—': 0x97, '–': 0x96, '’': 0x92, '“': 0x93, '”': 0x94 };

// Approximate AFM advance widths (1/1000 em). Good enough for wrapping and for
// computing expected rects (tests use generous tolerances).
const W_HELV = widthTable(
  ' 278 ! 278 " 355 # 556 $ 556 % 889 & 667 \' 191 ( 333 ) 333 * 389 + 584 , 278 - 333 . 278 / 278 : 278 ; 278 = 584 ? 556 ' +
    'A 667 B 667 C 722 D 722 E 667 F 611 G 778 H 722 I 278 J 500 K 667 L 556 M 833 N 722 O 778 P 667 Q 778 R 722 S 667 T 611 U 722 V 667 W 944 X 667 Y 667 Z 611 ' +
    'a 556 b 556 c 500 d 556 e 556 f 278 g 556 h 556 i 222 j 222 k 500 l 222 m 833 n 556 o 556 p 556 q 556 r 333 s 500 t 278 u 556 v 500 w 722 x 500 y 500 z 500',
  556,
);
const W_HELV_BOLD = widthTable(
  ' 278 . 278 , 278 : 333 - 333 ' +
    'A 722 B 722 C 722 D 722 E 667 F 611 G 778 H 722 I 278 J 556 K 722 L 611 M 833 N 722 O 778 P 667 Q 778 R 722 S 667 T 611 U 722 V 667 W 944 X 667 Y 667 Z 611 ' +
    'a 556 b 611 c 556 d 611 e 556 f 333 g 611 h 611 i 278 j 278 k 556 l 278 m 889 n 611 o 611 p 611 q 611 r 389 s 556 t 333 u 611 v 556 w 778 x 556 y 556 z 500',
  556,
);
const W_TIMES = widthTable(
  ' 250 ! 333 " 408 ( 333 ) 333 , 250 - 333 . 250 / 278 : 278 ; 278 = 564 ? 444 ' +
    'A 722 B 667 C 667 D 722 E 611 F 556 G 722 H 722 I 333 J 389 K 722 L 611 M 889 N 722 O 722 P 556 Q 722 R 667 S 556 T 611 U 722 V 722 W 944 X 722 Y 722 Z 611 ' +
    'a 444 b 500 c 444 d 500 e 444 f 333 g 500 h 500 i 278 j 278 k 500 l 278 m 778 n 500 o 500 p 500 q 500 r 333 s 389 t 278 u 500 v 500 w 722 x 500 y 500 z 444',
  500,
);
function widthTable(spec, digitWidth) {
  const t = new Map();
  // spec is "<char> <width>" pairs separated by single spaces; the space char
  // itself is encoded as a leading " 278".
  const toks = spec.split(' ');
  for (let i = 0; i < toks.length; i++) {
    if (toks[i] === '' && /^\d+$/.test(toks[i + 1] ?? '')) {
      t.set(' ', Number(toks[i + 1]));
      i++;
      continue;
    }
    const ch = toks[i];
    const w = Number(toks[i + 1]);
    if (ch && Number.isFinite(w)) {
      t.set(ch, w);
      i++;
    }
  }
  for (const d of '0123456789') t.set(d, digitWidth);
  return t;
}
function textWidth(s, font, size) {
  if (font === 'F5') return s.length * 600 * size / 1000; // Courier
  const table = font === 'F2' || font === 'F7' ? W_HELV_BOLD : font === 'F1' ? W_HELV : W_TIMES;
  let w = 0;
  for (const ch of s) w += table.get(ch) ?? 500;
  return (w * size) / 1000;
}

/** Wrap words to a width; a "|" inside a word marks an allowed hyphenation point. */
function wrap(text, font, size, width) {
  const words = text.split(/\s+/).filter(Boolean);
  const lines = [];
  let cur = '';
  for (let wi = 0; wi < words.length; wi++) {
    const word = words[wi];
    if (word.includes('~')) {
      // forced line break inside the word ("recon~struction" → "recon-" / "struction")
      const [h, tail] = word.split('~');
      const head = h.endsWith('-') ? h : `${h}-`;
      const cand = cur ? `${cur} ${head}` : head;
      if (textWidth(cand, font, size) <= width) lines.push(cand);
      else {
        if (cur) lines.push(cur);
        lines.push(head);
      }
      cur = tail;
      continue;
    }
    const plain = word.replaceAll('|', '');
    const cand = cur ? `${cur} ${plain}` : plain;
    if (textWidth(cand, font, size) <= width) {
      cur = cand;
      continue;
    }
    // try hyphenation
    const parts = word.split('|');
    let placed = false;
    for (let k = parts.length - 1; k >= 1 && cur; k--) {
      const head = parts.slice(0, k).join('') + '-';
      const tail = parts.slice(k).join('');
      if (textWidth(`${cur} ${head}`, font, size) <= width) {
        lines.push(`${cur} ${head}`);
        cur = tail;
        placed = true;
        break;
      }
    }
    if (placed) continue;
    if (cur) lines.push(cur);
    cur = plain;
  }
  if (cur) lines.push(cur);
  return lines;
}

/** The expected source text for a wrapped paragraph (hyphenation removed). */
function expectedText(text) {
  return text.replaceAll('|', '').replaceAll('~', '').split(/\s+/).filter(Boolean).join(' ');
}

// ---------------------------------------------------------------------------
// Page composition helpers
// ---------------------------------------------------------------------------

class Page {
  constructor({ mediaBox = [0, 0, 612, 792], cropBox = null, rotate = 0 } = {}) {
    this.mediaBox = mediaBox;
    this.cropBox = cropBox;
    this.rotate = rotate;
    this.ops = [];
    this.images = {};
    this.expected = []; // expected blocks in reading order
  }
  text(font, size, x, y, str, { matrix = null, rise = 0, wordSpacing = 0 } = {}) {
    const m = matrix ?? [1, 0, 0, 1, x, y];
    this.ops.push(
      `BT /${font} ${num(size)} Tf ${num(wordSpacing)} Tw ${num(rise)} Ts ${m.map(num).join(' ')} Tm ${pdfString(str)} Tj ET`,
    );
  }
  raw(op) {
    this.ops.push(op);
  }
  /**
   * Lays out a paragraph; returns the next baseline y.
   * Records an expected block {kind, text, rect:[x0,y0,x1,y1]} unless `expect` is false.
   */
  paragraph(text, { font = 'F3', size = 11, x = 72, width = 468, y, leading = size * 1.27, indent = 0,
    justify = false, kind = 'paragraph', roles = undefined, policy = undefined, expect = true, hanging = null } = {}) {
    const lines = wrap(text, font, size, width - Math.max(indent, hanging ? hanging.indent : 0));
    let by = y;
    let maxX = x;
    lines.forEach((line, i) => {
      let lx = x + (i === 0 ? indent : 0);
      const str = line;
      if (hanging && i === 0) {
        this.text(font, size, x, by, hanging.marker);
        lx = x + hanging.indent;
      } else if (hanging) {
        lx = x + hanging.indent;
      }
      const lineWidth = width - (lx - x);
      let ws = 0;
      const natural = textWidth(str, font, size);
      const spaces = (str.match(/ /g) ?? []).length;
      if (justify && i < lines.length - 1 && spaces > 0) ws = (lineWidth - natural) / spaces;
      this.text(font, size, lx, by, str, { wordSpacing: ws });
      maxX = Math.max(maxX, lx + natural + ws * spaces);
      by -= leading;
    });
    if (expect) {
      const full = hanging ? `${hanging.marker} ${expectedText(text)}` : expectedText(text);
      this.expected.push({
        kind,
        ...(roles ? { roles } : {}),
        ...(policy ? { policy } : {}),
        text: full,
        rect: [x, y - (lines.length - 1) * leading - size * 0.25, maxX, y + size * 0.75].map(round1),
        lines: lines.length,
      });
    }
    return { y: by, lines, maxX };
  }
  line(font, size, x, y, str, kind = 'paragraph', extra = {}) {
    this.text(font, size, x, y, str);
    this.expected.push({ kind, ...extra, text: str, rect: [x, y - size * 0.25, x + textWidth(str, font, size), y + size * 0.75].map(round1), lines: 1 });
  }
  image(name, { x, y, w, h }) {
    this.images[name] = true;
    this.ops.push(`q ${num(w)} 0 0 ${num(h)} ${num(x)} ${num(y)} cm /${name} Do Q`);
  }
}

const num = (v) => (Number.isInteger(v) ? String(v) : v.toFixed(3).replace(/0+$/, '').replace(/\.$/, ''));
const round1 = (v) => Math.round(v * 10) / 10;

function writePdf(name, pages, { extraFonts = null } = {}) {
  const w = new PdfWriter();
  const catalog = w.alloc();
  const pagesId = w.alloc();
  const fontIds = {};
  for (const [key, base] of Object.entries(STD_FONTS)) {
    const enc = base === 'Symbol' ? '' : ' /Encoding /WinAnsiEncoding';
    fontIds[key] = w.add(`<< /Type /Font /Subtype /Type1 /BaseFont /${base}${enc} >>`);
  }
  if (extraFonts) Object.assign(fontIds, extraFonts(w));
  // 8x8 gray "scan" pattern image
  const pixels = Buffer.alloc(64);
  for (let i = 0; i < 64; i++) pixels[i] = ((i * 37) % 200) + 30;
  const imageId = w.add(w.stream('/Type /XObject /Subtype /Image /Width 8 /Height 8 /ColorSpace /DeviceGray /BitsPerComponent 8', pixels));
  const fontDict = Object.entries(fontIds).map(([k, id]) => `/${k} ${id} 0 R`).join(' ');
  const kids = [];
  for (const p of pages) {
    const content = w.add(w.stream('', p.ops.join('\n')));
    const xobj = Object.keys(p.images).length
      ? ` /XObject << ${Object.keys(p.images).map((n) => `/${n} ${imageId} 0 R`).join(' ')} >>`
      : '';
    const crop = p.cropBox ? ` /CropBox [${p.cropBox.join(' ')}]` : '';
    const rot = p.rotate ? ` /Rotate ${p.rotate}` : '';
    kids.push(
      w.add(`<< /Type /Page /Parent ${pagesId} 0 R /MediaBox [${p.mediaBox.join(' ')}]${crop}${rot} /Resources << /Font << ${fontDict} >>${xobj} >> /Contents ${content} 0 R >>`),
    );
  }
  w.set(pagesId, `<< /Type /Pages /Kids [${kids.map((k) => `${k} 0 R`).join(' ')}] /Count ${kids.length} >>`);
  w.set(catalog, `<< /Type /Catalog /Pages ${pagesId} 0 R >>`);
  writeFileSync(join(OUT, `${name}.pdf`), w.build(catalog));
  const expected = {
    fixture: `${name}.pdf`,
    note: 'Generated by scripts/fixtures/make-pdf-fixtures.mjs. Rects are approximate PDF user-space [x0,y0,x1,y1] (AFM-estimated widths); compare with tolerance.',
    pages: pages.map((p, i) => ({
      pageIndex: i,
      viewBox: p.cropBox ?? p.mediaBox,
      rotation: p.rotate,
      ...(p.meta ?? {}),
      blocks: p.expected,
    })),
  };
  writeFileSync(join(OUT, `${name}.expected.json`), JSON.stringify(expected, null, 2) + '\n');
}

// ---------------------------------------------------------------------------
// Text corpus (original text, written for these fixtures)
// ---------------------------------------------------------------------------

const P = {
  intro1:
    'Reading a PDF is not the same as reading its text. The file stores glyphs at absolute positions, and nothing in the format says which line follows which or where a paragraph ends. A reader that wants to reflow the text must rebuild that structure from geometry alone, line by line and column by column.',
  intro2:
    'This fixture exercises the simplest case: a single column of body text with a title, a few headings and a footnote. Even here the recon~struction has to cope with a superscript marker, a running header that repeats on every page, and page numbers that change from one page to the next.',
  method1:
    'Lines are formed by grouping glyph runs that share a baseline and a font size. Runs that sit slightly above the baseline, such as footnote markers, are attached to the line they decorate instead of starting a new one. Compound names such as Thoth-~Reader keep their hyphen when the line is joined.',
  method2:
    'Paragraphs are then assembled from consecutive lines using vertical spacing, indentation and punctuation. A large gap or an indented first line starts a new paragraph, and a short line that ends a sentence usually closes one.',
  list1: 'Glyph runs are normalised so that rotated text is read along its own baseline.',
  list2: 'Columns are separated by looking for tall rivers of white space between text.',
  list3: 'Headings, captions and footnotes are classified from font size and position.',
  cont1:
    'The final paragraph on this page is deliberately cut in the middle of a sentence so that the extractor can mark it as a candidate for continuation on the next page, without merging the two halves into',
  cont2:
    'one block. Merging across pages is left to a later version, because a wrong merge is much harder to undo than a missing one.',
  results1:
    'On these synthetic pages the reading order is fully determined by the layout, which makes them useful as regression tests rather than as a measure of real-world quality.',
  num1: 'Every block keeps the rectangles of its lines in PDF user space.',
  num2: 'Every block records which text items and which code units it came from.',
  last1:
    'The last page is short. It only exists so that the running header and the page number appear on at least three pages, which is the minimum the extractor needs before it calls something a running element.',
  footnote:
    'Footnotes are set in a smaller type at the bottom of the page and should be kept separate from the body text above them.',
};

function header(page, n) {
  page.line('F1', 9, 72, 756, 'Thoth Reader Fixture — Layout Recovery Notes', 'paragraph', { roles: ['running_header'], policy: 'preserve' });
  page.footerText = `Page ${n}`;
}
function footer(page) {
  page.line('F1', 9, 288, 36, page.footerText, 'paragraph', { roles: ['page_number', 'running_footer'], policy: 'preserve' });
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function singleColumn() {
  const pages = [];
  // page 1
  let p = new Page();
  header(p, 1);
  p.line('F2', 20, 72, 700, 'Recovering Reading Order from PDF Text', 'heading');
  p.line('F2', 13, 72, 664, '1 Introduction', 'heading');
  // paragraph with superscript footnote marker appended to its first line
  let r = p.paragraph(P.intro1, { y: 640 });
  const firstLine = r.lines[0];
  p.text('F3', 7, 72 + textWidth(firstLine, 'F3', 11) + 0.5, 640 + 4, '1');
  p.expected.at(-1).text = expectedText(P.intro1).replace(firstLine, firstLine + '1');
  r = p.paragraph(P.intro2, { y: r.y - 8 });
  // footnote block (marker + text) near the bottom
  p.text('F3', 6, 72, 92 + 3, '1');
  const fn = p.paragraph(P.footnote, { font: 'F3', size: 8, x: 78, width: 462, y: 92, expect: false });
  p.expected.push({ kind: 'paragraph', roles: ['footnote'], text: `1 ${expectedText(P.footnote)}`, rect: [72, round1(fn.y + 10 - 2), round1(fn.maxX), 98], lines: fn.lines.length });
  footer(p);
  pages.push(p);

  // page 2
  p = new Page();
  header(p, 2);
  p.line('F2', 13, 72, 700, '2 Method', 'heading');
  r = p.paragraph(P.method1, { y: 676 });
  r = p.paragraph(P.method2, { y: r.y - 8 });
  let y = r.y - 8;
  for (const t of [P.list1, P.list2, P.list3]) {
    r = p.paragraph(t, { y, kind: 'list_item', hanging: { marker: '•', indent: 14 } });
    y = r.y - 4;
  }
  r = p.paragraph(P.cont1, { y: y - 4, roles: ['continues_on_next_page'] });
  footer(p);
  pages.push(p);

  // page 3
  p = new Page();
  header(p, 3);
  r = p.paragraph(P.cont2, { y: 700 });
  p.line('F2', 13, 72, r.y - 14, '3 Results', 'heading');
  r = p.paragraph(P.results1, { y: r.y - 38 });
  y = r.y - 8;
  for (const [i, t] of [P.num1, P.num2].entries()) {
    r = p.paragraph(t, { y, kind: 'list_item', hanging: { marker: `${i + 1}.`, indent: 16 } });
    y = r.y - 4;
  }
  footer(p);
  pages.push(p);

  // page 4
  p = new Page();
  header(p, 4);
  p.paragraph(P.last1, { y: 700 });
  footer(p);
  pages.push(p);
  writePdf('single-column', pages);
}

const TC = {
  abstract:
    'Abstract. Two-column layouts are common in papers and magazines. The extractor must read the left column completely before the right one, and it must never interleave lines that merely share a baseline.',
  l1: 'Multi-column pages place two or more text columns side by side. Their lines are often aligned to a common baseline grid, so a naive reader that sorts glyphs by height alone produces sentences that jump across the gutter on every line.',
  l2: 'The gutter between columns is a vertical band of white space that runs through most of the page. Finding it is the first step towards a correct reading order for the body text on the page.',
  r1: 'Once the gutter is known, every line can be assigned to the column on its side. Lines that cross the gutter, such as a title or a wide figure caption, split the page into zones that are read from top to bottom.',
  r2: 'Inside each zone the left column is read first and the right column second. Within a column the lines simply follow each other from top to bottom, and paragraphs are separated by indentation.',
  z2l: 'Below the figure the two columns resume. This second zone must be read after the caption and must again keep the left column before the right column.',
  z2r: 'The right column of the second zone closes the page. Its first line shares a baseline with the first line of the left column in the same zone.',
};

function twoColumn() {
  const pages = [];
  const L = { x: 72, width: 225 };
  const R = { x: 315, width: 225 };
  const body = { font: 'F3', size: 10, leading: 12, justify: true, indent: 12 };
  // page 1
  let p = new Page();
  p.line('F2', 18, 130, 720, 'Reading Order in Multi-Column Pages', 'heading');
  p.line('F1', 10, 245, 700, 'Thoth Fixture Authors', 'paragraph');
  let r = p.paragraph(TC.abstract, { font: 'F4', size: 10, leading: 12, x: 72, width: 468, y: 676 });
  const top = r.y - 14;
  // left column
  p.line('F2', 11, L.x, top, '1 Introduction', 'heading');
  r = p.paragraph(TC.l1, { ...body, ...L, y: top - 16 });
  r = p.paragraph(TC.l2, { ...body, ...L, y: r.y });
  // right column, same baseline grid
  r = p.paragraph(TC.r1, { ...body, ...R, y: top, indent: 0 });
  p.line('F2', 11, R.x, r.y - 4, '2 Background', 'heading');
  r = p.paragraph(TC.r2, { ...body, ...R, y: r.y - 20 });
  p.line('F1', 9, 300, 36, '1', 'paragraph', { roles: ['page_number'], policy: 'preserve' });
  pages.push(p);

  // page 2: zone 1 (two columns), spanning figure + caption, zone 2 (two columns)
  p = new Page();
  r = p.paragraph(TC.l2, { ...body, ...L, y: 720 });
  const zl = r.y;
  r = p.paragraph(TC.r2, { ...body, ...R, y: 720 });
  const zEnd = Math.min(zl, r.y);
  p.image('Im1', { x: 72, y: zEnd - 170, w: 468, h: 150 });
  // The figure image itself is not a text block (v1 emits no figure blocks).
  p.meta = { imageRects: [[72, zEnd - 170, 540, zEnd - 20]] };
  const capY = zEnd - 190;
  p.line('F1', 9, 72, capY, 'Figure 1: A wide caption that crosses the gutter splits the page into two zones of columns.', 'caption');
  r = p.paragraph(TC.z2l, { ...body, ...L, y: capY - 24 });
  r = p.paragraph(TC.z2r, { ...body, ...R, y: capY - 24 });
  p.line('F1', 9, 300, 36, '2', 'paragraph', { roles: ['page_number'], policy: 'preserve' });
  pages.push(p);
  writePdf('two-column', pages);
}

function tableFormula() {
  const p = new Page();
  p.line('F2', 14, 72, 720, 'Tables, Formulas and Code', 'heading');
  let r = p.paragraph(
    'Tables and formulas are not reflowed. They are kept as preserved blocks so that their layout is shown from the original page and they are never sent for automatic translation.',
    { y: 696 },
  );
  p.line('F1', 9, 72, r.y - 6, 'Table 1: Synthetic measurements used by the extractor tests.', 'caption');
  const rows = [
    ['Metric', 'Target', 'Measured'],
    ['Precision', '0.98', '1.00'],
    ['Recall', '0.95', '1.00'],
    ['Order accuracy', '0.98', '1.00'],
  ];
  let y = r.y - 26;
  const tableTop = y;
  const cols = [90, 260, 400];
  for (const [ri, row] of rows.entries()) {
    row.forEach((cell, ci) => p.text(ri === 0 ? 'F2' : 'F1', 9, cols[ci], y, cell));
    y -= 14;
  }
  p.expected.push({
    kind: 'table',
    policy: 'preserve',
    text: rows.map((row) => row.join(' ')).join(' '),
    rect: [90, y + 14 - 2.5, 450, tableTop + 7],
    lines: rows.length,
  });
  r = p.paragraph('The formula below is a single display line made of italic letters, symbols and an equation number.', { y: y - 12 });
  // formula: f(x) = Σ a_i x^i   (1)
  const fy = r.y - 10;
  p.text('F4', 12, 220, fy, 'f');
  p.text('F3', 12, 225, fy, '(');
  p.text('F4', 12, 229, fy, 'x');
  p.text('F3', 12, 235, fy, ') =');
  p.text('F6', 14, 255, fy, 'S'); // Symbol 'S' = Σ
  p.text('F4', 12, 268, fy, 'a');
  p.text('F4', 8, 274, fy - 3, 'i');
  p.text('F4', 12, 279, fy, 'x');
  p.text('F4', 8, 285, fy + 5, 'i');
  p.text('F3', 12, 500, fy, '(1)');
  p.expected.push({ kind: 'formula', policy: 'preserve', text: 'f(x) = Σai xi (1)', rect: [220, fy - 6, 515, fy + 12], lines: 1, matchPrefix: 'f' });
  r = p.paragraph('A short code listing follows; its indentation must survive extraction.', { y: fy - 28 });
  const code = ['function add(a, b) {', '    return a + b;', '}'];
  y = r.y - 6;
  for (const c of code) {
    p.text('F5', 9, 90, y, c);
    y -= 11;
  }
  p.expected.push({ kind: 'code', policy: 'preserve', text: code.join('\n'), rect: [90, y + 11 - 2.25, 90 + 20 * 5.4, r.y - 6 + 6.75], lines: 3, raw: true });
  p.paragraph('After the code the prose resumes and should be translated normally.', { y: y - 10 });
  writePdf('table-formula', [p]);
}

function rotated() {
  const pages = [];
  // page 0: /Rotate 90, text upright in user space (displays sideways)
  let p = new Page({ rotate: 90 });
  p.line('F2', 14, 72, 720, 'A Page Rotated by Ninety Degrees', 'heading');
  let r = p.paragraph('This page carries a Rotate entry of ninety degrees. Its text is upright in user space, so the extractor reads it normally while the viewer shows the page turned clockwise.', { y: 696 });
  p.paragraph('Rectangles are stored in user space and converted to screen pixels with the viewport of the rotated page.', { y: r.y - 8 });
  pages.push(p);

  // page 1: upright page with a rotated margin note (text matrix rotated +90)
  p = new Page();
  p.line('F2', 14, 72, 720, 'Mixed Orientation', 'heading');
  p.paragraph('The body of this page is upright. A short note runs up the left margin; it is extracted as its own block and flagged as rotated text.', { y: 696 });
  p.text('F1', 10, 0, 0, 'DRAFT margin note running upwards', { matrix: [0, 1, -1, 0, 50, 300] });
  p.expected.push({ kind: 'paragraph', text: 'DRAFT margin note running upwards', rect: [41, 300, 52, round1(300 + textWidth('DRAFT margin note running upwards', 'F1', 10))], lines: 1, rotatedText: true, policy: 'manual_only' });
  pages.push(p);

  // page 2: /Rotate 90 with text drawn rotated +90 => upright on screen
  p = new Page({ rotate: 90 });
  const m = (x, y) => [0, 1, -1, 0, x, y];
  // baselines run up (direction +y), successive lines move towards -x
  // Glyph "up" is -x, so later lines (further down on screen) have larger x.
  p.text('F2', 14, 0, 0, 'Landscape Content', { matrix: m(60, 72) });
  p.text('F3', 11, 0, 0, 'This landscape page is drawn with a rotated text matrix', { matrix: m(90, 72) });
  p.text('F3', 11, 0, 0, 'and a Rotate entry, so it reads upright on screen.', { matrix: m(104, 72) });
  // rect in user space: glyphs extend from x - ascent·size (up) to x - descent·size
  const rot = (x, size, str, font) => [round1(x - 0.85 * size), 72, round1(x + 0.22 * size), round1(72 + textWidth(str, font, size))];
  p.expected.push({ kind: 'heading', text: 'Landscape Content', rect: rot(60, 14, 'Landscape Content', 'F2'), lines: 1 });
  const r1 = rot(90, 11, 'This landscape page is drawn with a rotated text matrix', 'F3');
  const r2 = rot(104, 11, 'and a Rotate entry, so it reads upright on screen.', 'F3');
  p.expected.push({ kind: 'paragraph', text: 'This landscape page is drawn with a rotated text matrix and a Rotate entry, so it reads upright on screen.', rect: [r1[0], 72, r2[2], Math.max(r1[3], r2[3])], lines: 2 });
  pages.push(p);
  writePdf('rotated', pages);
}

function cropBox() {
  const pages = [];
  let p = new Page({ cropBox: [50, 100, 562, 742] });
  p.line('F2', 14, 80, 700, 'Cropped Page', 'heading');
  p.paragraph('The crop box of this page starts at fifty and one hundred points, so the view box is not anchored at the origin. Text outside the crop box is invisible and must not become a block.', { y: 676, x: 80, width: 440 });
  p.text('F1', 10, 80, 60, 'HIDDEN TEXT OUTSIDE THE CROP BOX');
  pages.push(p);
  p = new Page({ cropBox: [30, 40, 590, 760], rotate: 180 });
  p.line('F2', 14, 80, 700, 'Cropped and Upside Down', 'heading');
  p.paragraph('This page combines a crop box offset with a rotation of one hundred and eighty degrees, which is the hardest case for converting between screen pixels and user space.', { y: 676, x: 80, width: 440 });
  pages.push(p);
  p = new Page({ cropBox: [20, 30, 600, 770], rotate: 270 });
  p.line('F2', 14, 80, 700, 'Cropped and Rotated Left', 'heading');
  p.paragraph('A crop box offset together with a rotation of two hundred and seventy degrees.', { y: 676, x: 80, width: 440 });
  pages.push(p);
  writePdf('cropbox', pages);
}

function scan() {
  const p = new Page();
  p.image('Im1', { x: 0, y: 0, w: 612, h: 792 });
  p.meta = { likelyScan: true };
  writePdf('scan', [p]);
}

function unmapped() {
  // Type3 font whose glyph names are meaningless and which has no ToUnicode:
  // pdf.js cannot recover Unicode for these glyphs.
  const extraFonts = (w) => {
    const procs = [];
    for (let i = 0; i < 6; i++) procs.push(w.add(w.stream('', `600 0 0 0 500 700 d1\n0 0 ${300 + i * 30} 700 re f`)));
    const names = procs.map((_, i) => `/qzx${i}`).join(' ');
    const charProcs = procs.map((id, i) => `/qzx${i} ${id} 0 R`).join(' ');
    const t3 = w.add(
      `<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << ${charProcs} >> /Encoding << /Type /Encoding /Differences [1 ${names}] >> /FirstChar 1 /LastChar 6 /Widths [600 600 600 600 600 600] /Resources << >> >>`,
    );
    return { T3: t3 };
  };
  const p = new Page();
  const glyphs = (n, seed) => {
    let s = '';
    for (let i = 0; i < n; i++) s += `\\00${((i * 7 + seed) % 6) + 1}`;
    return s;
  };
  let y = 700;
  const texts = [];
  for (let line = 0; line < 6; line++) {
    p.raw(`BT /T3 11 Tf 1 0 0 1 72 ${y} Tm (${glyphs(60, line)}) Tj ET`);
    let t = '';
    for (let i = 0; i < 60; i++) t += String.fromCharCode(((i * 7 + line) % 6) + 1);
    texts.push(t);
    y -= 14;
  }
  // pdf.js passes the unmapped char codes through as C0 controls: one low-quality block
  p.expected.push({ kind: 'unknown', policy: 'manual_only', text: texts.join(' '), rect: [72, round1(y + 14 - 2.2), 72 + 396, 700 + 8.8], lines: 6 });
  p.meta = { missingUnicode: true };
  writePdf('unmapped', [p], { extraFonts });
}

mkdirSync(OUT, { recursive: true });
singleColumn();
twoColumn();
tableFormula();
rotated();
cropBox();
scan();
unmapped();
console.log(`wrote fixtures to ${OUT}`);
