//! De-identified diagnostics. Never log Authorization headers, request bodies,
//! source/translated text, URL queries or full local paths.

pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(512));
    let mut redact_next = false;
    for token in s.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let was_redact_next = std::mem::replace(&mut redact_next, lower.starts_with("bearer") || lower.starts_with("authorization"));
        let t = if was_redact_next || lower.starts_with("sk-") || lower.starts_with("bearer") || lower.contains("authorization") || lower.contains("api_key") || lower.contains("apikey") {
            "[redacted]".to_string()
        } else if token.starts_with('/') && token.matches('/').count() > 1 {
            // absolute path -> keep only file name
            format!("…/{}", token.rsplit('/').next().unwrap_or(""))
        } else if let Some(i) = token.find('?') {
            if token.starts_with("http") { format!("{}?[redacted]", &token[..i]) } else { token.to_string() }
        } else {
            token.to_string()
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&t);
        if out.len() > 512 {
            out.truncate(512);
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
}
