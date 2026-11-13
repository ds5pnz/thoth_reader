//! RFC 8785 (JCS) style deterministic JSON serialization.
//! Object keys are sorted by UTF-16 code units, no insignificant whitespace,
//! numbers use the ECMAScript shortest form. All cache/id hashing goes through here.

use serde_json::Value;

pub fn to_canonical_string(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

pub fn canonical_sha256(v: &Value) -> String {
    super::sha256_hex(to_canonical_string(v).as_bytes())
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => out.push_str(&serde_json::to_string(s).expect("string serialization")),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("key serialization"));
                out.push(':');
                write_value(&m[*k], out);
            }
            out.push('}');
        }
    }
}

fn write_number(n: &serde_json::Number, out: &mut String) {
    if let Some(i) = n.as_i64() {
        out.push_str(&i.to_string());
        return;
    }
    if let Some(u) = n.as_u64() {
        out.push_str(&u.to_string());
        return;
    }
    let f = n.as_f64().unwrap_or(0.0);
    out.push_str(&es_number_to_string(f));
}

/// ECMAScript Number::toString for finite doubles.
pub fn es_number_to_string(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    if !f.is_finite() {
        // JSON cannot carry these; callers must never pass them.
        return "null".into();
    }
    let abs = f.abs();
    if abs.fract() == 0.0 && abs < 1e21 {
        return format!("{}", f as i128);
    }
    if (1e-6..1e21).contains(&abs) {
        // Rust's Display gives shortest round-trip without exponent in this range.
        return format!("{}", f);
    }
    // Exponent form: d[.ddd]e[+-]n
    let s = format!("{:e}", f); // e.g. 1.5e-7
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    format!("{}e{}{}", mant, if exp >= 0 { "+" } else { "-" }, exp.abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn golden_key_order_and_numbers() {
        let v = json!({"b": 1, "a": [3, null, true], "\u{e9}": "x", "A": 1.5, "z": {"y": 0, "x": -0.0}});
        assert_eq!(
            to_canonical_string(&v),
            r#"{"A":1.5,"a":[3,null,true],"b":1,"z":{"x":0,"y":0},"é":"x"}"#
        );
    }

    #[test]
    fn golden_utf16_sort() {
        // U+1F600 (surrogates D83D..) sorts before U+FF61 in UTF-16 order, after it in code points.
        let v = json!({"\u{ff61}": 1, "\u{1f600}": 2});
        assert_eq!(to_canonical_string(&v), "{\"\u{1f600}\":2,\"\u{ff61}\":1}");
    }

    #[test]
    fn golden_escapes() {
        let v = json!({"k": "a\"b\\c\n\u{1}/"});
        assert_eq!(to_canonical_string(&v), r#"{"k":"a\"b\\c\n\u0001/"}"#);
    }

    #[test]
    fn es_numbers() {
        assert_eq!(es_number_to_string(1e21), "1e+21");
        assert_eq!(es_number_to_string(1.5e-7), "1.5e-7");
        assert_eq!(es_number_to_string(123.0), "123");
        assert_eq!(es_number_to_string(0.1), "0.1");
    }

    #[test]
    fn golden_hash_is_stable() {
        let v = json!({"keyVersion": 1, "blockId": "abc"});
        assert_eq!(
            canonical_sha256(&v),
            crate::domain::sha256_hex(br#"{"blockId":"abc","keyVersion":1}"#)
        );
    }
}
