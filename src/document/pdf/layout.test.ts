import { describe, expect, it } from 'vitest';
import { analyzeLayout, frameBoxToUserRect, fromFrame, makeFrame, orientationKey, toFrame, type Line, type PageLayout } from './layout';
import { filler, item, item90, pageInput, para } from './testing/synthetic';

const texts = (lay: PageLayout) =>
  lay.dominant!.paragraphs.map((p) =>
    p.units
      .flatMap((u) => u.lines)
      .map((l: Line) => l.text)
      .join(' | '),
  );

/** n lines of ~45 chars (225pt at 10pt) starting at y, leading 12. */
function column(seed: number, x: number, y: number, n: number, indentFirst = 0): ReturnType<typeof para> {
  const lines = Array.from({ length: n }, (_, i) => filler(seed * 31 + i, 45));
  return para(lines, x, y, { leading: 12, indent: indentFirst });
}

describe('frames', () => {
  it('rotations are exact and invertible', () => {
    for (const a of [0, 90, 180, 270]) {
      const f = makeFrame(a);
      const [x, y] = toFrame(f, 123.5, 456.25);
      expect(fromFrame(f, x, y)).toEqual([123.5, 456.25]);
    }
    expect(toFrame(makeFrame(90), 10, 20)).toEqual([20, -10]);
  });
  it('snaps near-quarter angles', () => {
    expect(orientationKey(89.2)).toBe(90);
    expect(orientationKey(-90)).toBe(270);
    expect(orientationKey(30.4)).toBe(30);
  });
  it('frame boxes convert back to user-space rects', () => {
    expect(frameBoxToUserRect(makeFrame(90), 72, -60, 200, -50)).toEqual({ x: 50, y: 72, width: 10, height: 128 });
  });
});

describe('lines', () => {
  it('attaches superscripts to their line and keeps separate baselines apart', () => {
    const lay = analyzeLayout(
      pageInput([
        item('The marker follows', 72, 700, 11),
        item('1', 72 + 18 * 5.5 + 0.3, 704, 7),
        item('Next line of text', 72, 686, 11),
      ]),
    );
    const lines = lay.dominant!.lines;
    expect(lines.map((l) => l.text)).toEqual(['The marker follows1', 'Next line of text']);
    expect(lines[0]!.size).toBe(11);
  });

  it('inserts a separator only for visible gaps', () => {
    const lay = analyzeLayout(pageInput([item('alpha', 72, 700), item('beta', 72 + 25 + 3, 700), item('gamma', 72 + 25 + 3 + 20 + 0.2, 700)]));
    expect(lay.dominant!.lines[0]!.text).toBe('alpha betagamma');
  });

  it('drops exact duplicate glyph runs and flags overlap', () => {
    const lay = analyzeLayout(pageInput([item('Bold', 72, 700), item('Bold', 72.3, 700), item(' rest', 92, 700)]));
    const l = lay.dominant!.lines[0]!;
    expect(l.text).toBe('Bold rest');
    expect(l.duplicateCount).toBe(1);
  });
});

describe('columns', () => {
  it('reads two columns with shared baselines column by column (T02)', () => {
    const items = [...column(1, 72, 700, 8), ...column(2, 315, 700, 8)];
    const lay = analyzeLayout(pageInput(items));
    expect(lay.dominant!.columnCount).toBe(2);
    const order = lay.dominant!.flows.flatMap((f) => f.units.flatMap((u) => u.lines.map((l) => l.x0)));
    // all left lines first, then all right lines
    expect(order).toEqual([...Array(8).fill(72), ...Array(8).fill(315)]);
    // no line mixes the columns
    for (const l of lay.dominant!.lines) expect(l.x1 - l.x0).toBeLessThan(240);
    expect(lay.dominant!.flows.every((f) => !f.unresolved)).toBe(true);
  });

  it('splits zones at spanning elements (title, wide caption)', () => {
    const items = [
      item('A Title Spanning Both Columns Of The Page', 150, 740, 18, 'B'),
      ...column(1, 72, 700, 5),
      ...column(2, 315, 700, 5),
      item(filler(9, 90), 72, 620, 9, 'H'),
      ...column(3, 72, 590, 4),
      ...column(4, 315, 590, 4),
    ];
    const lay = analyzeLayout(pageInput(items));
    const seq = lay.dominant!.flows.map((f) => (f.spanning ? 'S' : f.column === 0 ? 'L' : 'R'));
    expect(seq).toEqual(['S', 'L', 'R', 'S', 'L', 'R']);
  });

  it('handles three columns recursively', () => {
    const col = (seed: number, x: number) => para(Array.from({ length: 6 }, (_, i) => filler(seed + i, 30)), x, 700, { leading: 12 });
    const lay = analyzeLayout(pageInput([...col(1, 50), ...col(2, 230), ...col(3, 410)]));
    const xs = lay.dominant!.flows.map((f) => f.units[0]!.x0);
    expect(xs).toEqual([50, 230, 410]);
    expect(lay.dominant!.columnCount).toBe(3);
  });

  it('marks an ambiguous line that intrudes into the other column as unresolved', () => {
    const items = [
      ...column(1, 72, 700, 6),
      ...column(2, 315, 700, 6),
      // left-column line overhanging 40pt into the right column
      item(filler(5, 53), 72, 628, 10),
      ...column(3, 72, 616, 3),
      ...column(4, 315, 616, 3),
    ];
    const lay = analyzeLayout(pageInput(items));
    expect(lay.dominant!.flows.some((f) => f.unresolved)).toBe(true);
  });

  it('flags side-by-side short fragments (form/table-like) as unresolved', () => {
    const rows = Array.from({ length: 5 }, (_, i) => [item(`Key ${i}`, 72, 700 - i * 14), item(`value ${i}`, 300, 700 - i * 14)]).flat();
    const lay = analyzeLayout(pageInput(rows));
    expect(lay.dominant!.flows.every((f) => f.unresolved)).toBe(true);
  });
});

describe('tables', () => {
  it('groups aligned rows of short cells into one table unit', () => {
    const rows = [
      ['Metric', 'Target', 'Measured'],
      ['Precision', '0.98', '1.00'],
      ['Recall', '0.95', '0.97'],
    ];
    const items = rows.flatMap((r, ri) => r.map((c, ci) => item(c, 90 + ci * 150, 600 - ri * 14, 9)));
    const lay = analyzeLayout(pageInput([...para([filler(1, 80)], 72, 640), ...items]));
    const tables = lay.dominant!.units.filter((u) => u.kind === 'table');
    expect(tables).toHaveLength(1);
    expect(tables[0]!.rows!.map((r) => r.map((c) => c.text))).toEqual(rows);
  });

  it('does not treat two text columns as a table', () => {
    const lay = analyzeLayout(pageInput([...column(1, 72, 700, 6), ...column(2, 315, 700, 6)]));
    expect(lay.dominant!.units.some((u) => u.kind === 'table')).toBe(false);
  });
});

describe('paragraphs', () => {
  it('splits on first-line indentation (justified text)', () => {
    const p1 = para([filler(1, 45), filler(2, 45), filler(3, 45)], 72, 700, { leading: 12, indent: 12 });
    const p2 = para([filler(4, 45), filler(5, 30)], 72, 664, { leading: 12, indent: 12 });
    const lay = analyzeLayout(pageInput([...p1, ...p2]));
    expect(lay.dominant!.paragraphs.map((p) => p.units.length)).toEqual([3, 2]);
  });

  it('splits on extra vertical space and on short sentence-final lines', () => {
    const a = para([filler(1, 90), 'end of it.'], 72, 700, { leading: 12 });
    const b = para([filler(2, 90), filler(3, 90)], 72, 676, { leading: 12 });
    const c = para([filler(4, 90)], 72, 640, { leading: 12 });
    const lay = analyzeLayout(pageInput([...a, ...b, ...c]));
    expect(lay.dominant!.paragraphs.map((p) => p.units.length)).toEqual([2, 2, 1]);
  });

  it('starts a new item at each list marker and keeps hanging indents', () => {
    const items = [
      item('•', 72, 700),
      item(filler(1, 80), 86, 700),
      item(filler(2, 60), 86, 688),
      item('•', 72, 676),
      item(filler(3, 80), 86, 676),
      item('2.', 72, 664),
      item(filler(4, 40), 86, 664),
    ];
    const lay = analyzeLayout(pageInput(items));
    const t = texts(lay);
    expect(t).toHaveLength(3);
    expect(t[0]!.startsWith('•')).toBe(true);
    expect(t[0]!.split(' | ')).toHaveLength(2);
    expect(t[2]!.startsWith('2.')).toBe(true);
  });

  it('separates headings by font size / weight', () => {
    const items = [item('Heading', 72, 720, 14, 'B'), ...para([filler(1, 90), filler(2, 90)], 72, 700, { leading: 12 })];
    const lay = analyzeLayout(pageInput(items));
    expect(lay.dominant!.paragraphs.map((p) => p.units.length)).toEqual([1, 2]);
  });
});

describe('orientation', () => {
  it('builds lines along a rotated baseline and keeps the minor orientation separate', () => {
    const items = [...para([filler(1, 90), filler(2, 90)], 72, 700, { leading: 12 }), item90('margin note text', 40, 300)];
    const lay = analyzeLayout(pageInput(items));
    expect(lay.dominant!.frame.angle).toBe(0);
    const minor = lay.groups.find((g) => g.frame.angle === 90)!;
    expect(minor.lines[0]!.text).toBe('margin note text');
    const l = minor.lines[0]!;
    const r = frameBoxToUserRect(minor.frame, l.x0, l.bottom, l.x1, l.top);
    expect(r.x).toBeCloseTo(32, 5);
    expect(r.y).toBeCloseTo(300, 5);
    expect(r.width).toBeCloseTo(10, 5);
    expect(r.height).toBeCloseTo(80, 5);
  });

  it('orders rotated-page lines in reading order of their own frame', () => {
    // baseline up (+y), glyph up = -x: later lines have larger x
    const items = [item90('first line', 60, 72), item90('second line', 72, 72), item90('third line', 84, 72)];
    const lay = analyzeLayout(pageInput(items, { rotation: 90 }));
    expect(lay.dominant!.frame.angle).toBe(90);
    expect(lay.dominant!.lines.map((l) => l.text)).toEqual(['first line', 'second line', 'third line']);
  });
});

describe('clipping', () => {
  it('drops items fully outside the view box', () => {
    const lay = analyzeLayout(pageInput([item('inside', 100, 500), item('outside', 100, 40)], { viewBox: [50, 100, 562, 742] }));
    expect(lay.clippedItemCount).toBe(1);
    expect(lay.dominant!.lines.map((l) => l.text)).toEqual(['inside']);
  });
});
