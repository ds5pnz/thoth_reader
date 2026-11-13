use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

/// Incremental hasher for streaming file content.
pub struct StreamHasher(Sha256);

impl Default for StreamHasher {
    fn default() -> Self {
        Self(Sha256::new())
    }
}

impl StreamHasher {
    pub fn update(&mut self, b: &[u8]) {
        self.0.update(b);
    }
    pub fn finish(self) -> String {
        hex::encode(self.0.finalize())
    }
}
