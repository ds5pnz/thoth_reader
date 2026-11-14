/**
 * RFC 8785 (JCS) style canonical JSON, byte-for-byte compatible with the Rust
 * `domain::canonical_json::to_canonical_string`:
 *  - object keys sorted by UTF-16 code units (plain JS string comparison),
 *  - no insignificant whitespace,
 *  - strings escaped exactly like `JSON.stringify` (which matches serde_json for
 *    well-formed strings),
 *  - numbers in ECMAScript shortest form (`-0` → `0`); non-finite numbers are
 *    rejected because JSON cannot carry them.
 *
 * Every id / cache-key hash that must agree with Rust goes through this.
 */
export type CanonicalJsonValue =
  | null
  | boolean
  | number
  | string
  | readonly CanonicalJsonValue[]
  | { readonly [key: string]: CanonicalJsonValue | undefined };

export function canonicalJson(value: CanonicalJsonValue): string {
  const out: string[] = [];
  write(value, out);
  return out.join('');
}

function write(v: CanonicalJsonValue, out: string[]): void {
  if (v === null) {
    out.push('null');
    return;
  }
  switch (typeof v) {
    case 'boolean':
      out.push(v ? 'true' : 'false');
      return;
    case 'number':
      if (!Number.isFinite(v)) throw new TypeError('canonicalJson: non-finite number');
      // String(-0) === "0"; String(x) is the ECMAScript Number::toString shortest form.
      out.push(String(v));
      return;
    case 'string':
      if (hasLoneSurrogate(v)) throw new TypeError('canonicalJson: lone surrogate in string');
      out.push(JSON.stringify(v));
      return;
    case 'object': {
      if (Array.isArray(v)) {
        out.push('[');
        (v as readonly CanonicalJsonValue[]).forEach((x, i) => {
          if (i > 0) out.push(',');
          write(x, out);
        });
        out.push(']');
        return;
      }
      const obj = v as { readonly [key: string]: CanonicalJsonValue | undefined };
      // Default sort compares UTF-16 code units, which is exactly RFC 8785 order.
      const keys = Object.keys(obj).filter((k) => obj[k] !== undefined).sort();
      out.push('{');
      keys.forEach((k, i) => {
        if (i > 0) out.push(',');
        if (hasLoneSurrogate(k)) throw new TypeError('canonicalJson: lone surrogate in key');
        out.push(JSON.stringify(k), ':');
        write(obj[k] as CanonicalJsonValue, out);
      });
      out.push('}');
      return;
    }
    default:
      throw new TypeError(`canonicalJson: unsupported type ${typeof v}`);
  }
}

function hasLoneSurrogate(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const n = s.charCodeAt(i + 1);
      if (!(n >= 0xdc00 && n <= 0xdfff)) return true;
      i++;
    } else if (c >= 0xdc00 && c <= 0xdfff) {
      return true;
    }
  }
  return false;
}

/** Lower-case hex SHA-256 of the UTF-8 encoding of `text` (WebCrypto; works in workers, jsdom and Node). */
export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  const bytes = new Uint8Array(digest);
  let hex = '';
  for (const b of bytes) hex += b.toString(16).padStart(2, '0');
  return hex;
}
