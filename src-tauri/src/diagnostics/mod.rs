//! De-identified diagnostics. Never log Authorization headers, request bodies,
//! source/translated text, URL queries or full local paths.

/// Cut `s` to at most `max` bytes without splitting a character.
pub fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
}

fn is_secret_label(lower: &str) -> bool {
    let l = lower.trim_end_matches([':', '=']);
    l.ends_with("authorization") || l.ends_with("api-key") || l.ends_with("api_key") || l.ends_with("apikey") || l == "bearer" || l == "basic" || l == "token"
}

pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(512));
    let mut redact_next = false;
    for token in s.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let bare = lower.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/');
        // `Authorization: Bearer <cred>`, `x-api-key: <cred>`: the label and what follows it.
        let label = is_secret_label(bare);
        let was_redact_next = std::mem::replace(&mut redact_next, label);
        let secret = was_redact_next
            || label
            || lower.contains("sk-")
            || lower.contains("authorization")
            || lower.contains("api_key")
            || lower.contains("api-key")
            || lower.contains("apikey")
            || (lower.contains("://") && lower.contains('@'));
        let t = if secret {
            "[redacted]".to_string()
        } else if let Some(i) = bare.find('/').filter(|_| !bare.contains("://") && token.matches('/').count() > 1) {
            // absolute (possibly quoted) path -> keep only file name
            let _ = i;
            format!("…/{}", token.rsplit('/').next().unwrap_or(""))
        } else if let Some(i) = token.find('?').filter(|_| lower.contains("http")) {
            format!("{}?[redacted]", &token[..i])
        } else {
            token.to_string()
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&t);
        if out.len() > 512 {
            truncate_at_char(&mut out, 512);
            break;
        }
    }
    out
}

pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts_secrets_paths_queries() {
        let s = super::redact("Authorization: Bearer sk-abc /Users/me/secret/book.pdf https://h/v1?key=1");
        assert!(!s.contains("sk-abc"));
        assert!(!s.contains("/Users/me"));
        assert!(!s.contains("key=1"));
    }

    #[test]
    fn redacts_more_secret_shapes_and_never_splits_chars() {
        let s = super::redact("x-api-key: SECRET1 Authorization: Basic SECRET2 (https://h/v1?key=SECRET3) key=sk-SECRET4 https://u:SECRET5@h/p \"/Users/me/a/b.pdf\"");
        for n in 1..=5 {
            assert!(!s.contains(&format!("SECRET{n}")), "{s}");
        }
        assert!(!s.contains("/Users/me"), "{s}");
        // 3-byte characters straddling the 512-byte cut must not panic
        let long = "가".repeat(400);
        assert!(super::redact(&long).len() <= 512);
        let mut t = "가나다".to_string();
        super::truncate_at_char(&mut t, 4);
        assert_eq!(t, "가");
    }
}
