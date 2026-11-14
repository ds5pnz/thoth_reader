//! ZIP container guard (spec §5.1): limits are checked on the central directory
//! before anything is decompressed, and again on the real byte counts while reading.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use zip::{CompressionMethod, ZipArchive};

use super::paths::validate_entry_name;
use super::EpubLimits;
use crate::error::{AppError, ErrorCode};

/// Ratio checks only apply above these sizes so that tiny, highly compressible
/// files (e.g. a 5 KiB stylesheet of repeated rules) do not trip the 200:1 rule.
const RATIO_MIN_ENTRY_BYTES: u64 = 64 * 1024;
const RATIO_MIN_TOTAL_BYTES: u64 = 1024 * 1024;

pub(crate) fn reject(code: ErrorCode, reason: &str, msg: &str) -> AppError {
    AppError::new(code, msg).with("reason", reason)
}

pub(crate) fn unsupported(reason: &str) -> AppError {
    reject(
        ErrorCode::UnsupportedFormat,
        reason,
        "안전하지 않거나 손상된 EPUB 파일입니다. 다른 파일을 선택해 주세요.",
    )
}

pub(crate) fn limit(reason: &str) -> AppError {
    reject(
        ErrorCode::LimitExceeded,
        reason,
        "EPUB 파일이 허용 한도를 초과합니다. 더 작은 파일을 사용해 주세요.",
    )
}

pub(crate) fn drm() -> AppError {
    reject(
        ErrorCode::DrmUnsupported,
        "encrypted_content",
        "DRM 또는 암호화된 EPUB은 지원하지 않습니다.",
    )
}

pub(crate) struct SafeZip {
    archive: ZipArchive<File>,
    /// raw entry name -> index (regular files only)
    index: HashMap<String, usize>,
    /// NFC-normalized name -> raw name (fallback lookup for NFD/NFC mismatches)
    nfc_index: HashMap<String, String>,
    limits: EpubLimits,
    total_read: u64,
}

/// Read the entry count from the End Of Central Directory record (zip64 aware)
/// without letting the zip crate allocate the whole central directory.
fn eocd_entry_count(f: &mut File, file_len: u64) -> Option<u64> {
    let tail_len = file_len.min(65_535 + 22);
    f.seek(SeekFrom::Start(file_len - tail_len)).ok()?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).ok()?;
    let pos = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])?;
    let total = u16::from_le_bytes([tail[pos + 10], tail[pos + 11]]) as u64;
    if total != 0xFFFF {
        return Some(total);
    }
    // zip64 EOCD locator sits 20 bytes before the EOCD.
    if pos < 20 || tail[pos - 20..pos - 16] != [0x50, 0x4b, 0x06, 0x07] {
        return Some(total);
    }
    let off = u64::from_le_bytes(tail[pos - 12..pos - 4].try_into().ok()?);
    if off >= file_len {
        return None;
    }
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut rec = [0u8; 56];
    f.read_exact(&mut rec).ok()?;
    if rec[0..4] != [0x50, 0x4b, 0x06, 0x06] {
        return None;
    }
    Some(u64::from_le_bytes(rec[32..40].try_into().ok()?))
}

impl SafeZip {
    pub(crate) fn open(path: &Path, limits: &EpubLimits) -> Result<Self, AppError> {
        let mut file = File::open(path).map_err(|_| AppError::not_found("EPUB 파일"))?;
        let file_len = file.metadata().map_err(AppError::from)?.len();
        if file_len > limits.max_file_bytes {
            return Err(limit("file_too_large"));
        }
        match eocd_entry_count(&mut file, file_len) {
            Some(n) if n > limits.max_entries => return Err(limit("too_many_entries")),
            Some(_) => {}
            None => return Err(unsupported("not_a_zip")),
        }
        file.seek(SeekFrom::Start(0)).map_err(AppError::from)?;
        let archive = ZipArchive::new(file).map_err(|_| unsupported("not_a_zip"))?;
        if archive.len() as u64 > limits.max_entries {
            return Err(limit("too_many_entries"));
        }
        if archive.has_duplicate_names() {
            return Err(unsupported("duplicate_entry"));
        }

        let mut index = HashMap::new();
        let mut nfc_index = HashMap::new();
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut total_declared: u64 = 0;
        let mut total_compressed: u64 = 0;
        for i in 0..archive.len() {
            let e = archive.by_index_data(i).map_err(|_| unsupported("corrupt_central_directory"))?;
            let name = e.name().map_err(|_| unsupported("entry_name_encoding"))?.into_owned();
            if e.name_raw().contains(&0) {
                return Err(unsupported("path_nul"));
            }
            let key = validate_entry_name(name.trim_end_matches('/'))
                .map_err(|v| unsupported(v.code()))?;
            if let Some(mode) = e.unix_mode() {
                let kind = mode & 0o170000;
                if kind == 0o120000 {
                    return Err(unsupported("symlink_entry"));
                }
                if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                    return Err(unsupported("special_file_entry"));
                }
            }
            if !seen_keys.insert(key.clone()) {
                return Err(unsupported("duplicate_entry"));
            }
            if e.is_dir() {
                continue;
            }
            if e.encrypted() {
                return Err(drm());
            }
            match e.compression() {
                CompressionMethod::Stored | CompressionMethod::Deflated => {}
                _ => return Err(unsupported("unsupported_compression")),
            }
            let size = e.size();
            let csize = e.compressed_size();
            if size > limits.max_entry_bytes {
                return Err(limit("entry_too_large"));
            }
            if size >= RATIO_MIN_ENTRY_BYTES && size / csize.max(1) > limits.max_ratio {
                return Err(limit("compression_ratio"));
            }
            total_declared = total_declared.saturating_add(size);
            total_compressed = total_compressed.saturating_add(csize);
            if total_declared > limits.max_total_uncompressed {
                return Err(limit("total_uncompressed_too_large"));
            }
            use unicode_normalization::UnicodeNormalization;
            nfc_index.insert(name.nfc().collect::<String>(), name.clone());
            index.insert(name, i);
        }
        if total_declared >= RATIO_MIN_TOTAL_BYTES
            && total_declared / total_compressed.max(1) > limits.max_ratio
        {
            return Err(limit("compression_ratio"));
        }
        Ok(Self { archive, index, nfc_index, limits: limits.clone(), total_read: 0 })
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        if let Some(&i) = self.index.get(name) {
            return Some(i);
        }
        use unicode_normalization::UnicodeNormalization;
        let nfc: String = name.nfc().collect();
        self.nfc_index.get(&nfc).and_then(|raw| self.index.get(raw).copied())
    }

    /// Names of all (validated) file entries.
    pub(crate) fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.index.keys().cloned().collect();
        v.sort();
        v
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lookup(name).is_some()
    }

    /// Read a regular file entry, enforcing real byte limits (never trusting headers).
    pub(crate) fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, AppError> {
        let Some(i) = self.lookup(name) else { return Ok(None) };
        let cap = self.limits.max_entry_bytes;
        let remaining_total = self.limits.max_total_uncompressed.saturating_sub(self.total_read);
        let mut f = self.archive.by_index(i).map_err(|e| match e {
            zip::result::ZipError::UnsupportedArchive(msg) if msg.to_lowercase().contains("password") => drm(),
            _ => unsupported("corrupt_entry"),
        })?;
        let declared = f.size();
        let compressed = f.compressed_size().max(1);
        let hard = cap.min(remaining_total);
        let mut buf = Vec::with_capacity(declared.min(hard).min(4 * 1024 * 1024) as usize);
        let n = (&mut f)
            .take(hard + 1)
            .read_to_end(&mut buf)
            .map_err(|_| unsupported("corrupt_entry"))? as u64;
        if n > cap {
            return Err(limit("entry_too_large"));
        }
        if n > remaining_total {
            return Err(limit("total_uncompressed_too_large"));
        }
        if n > declared {
            return Err(unsupported("entry_size_mismatch"));
        }
        if n >= RATIO_MIN_ENTRY_BYTES && n / compressed > self.limits.max_ratio {
            return Err(limit("compression_ratio"));
        }
        self.total_read += n;
        Ok(Some(buf))
    }
}
