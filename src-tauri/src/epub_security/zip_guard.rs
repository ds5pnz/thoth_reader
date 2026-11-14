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

const EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const ZIP64_LOCATOR_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
const ZIP64_EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
const EOCD_LEN: usize = 22;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_EOCD_LEN: usize = 56;
/// Minimum size of one central directory record.
const CD_RECORD_MIN: u64 = 46;

fn le16(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes(s.try_into().ok()?) as u64)
}

fn le32(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?) as u64)
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}

/// Check the declared entry count before the zip crate sizes its tables from it.
///
/// The crate picks an End Of Central Directory record by scanning backwards (and
/// may fall back to an earlier signature), and switches to the zip64 record when
/// the 16-bit count, the 32-bit directory size *or* the 32-bit directory offset
/// is saturated. So every candidate record in the tail is checked, and for each
/// one that can go zip64 the 64-bit count is the one that has to be within limits.
fn check_declared_entries(f: &mut File, file_len: u64, max_entries: u64) -> Result<(), AppError> {
    let not_zip = || unsupported("not_a_zip");
    // EOCD + maximum comment, with a margin that covers the crate's slightly wider
    // search window and a zip64 locator in front of the earliest candidate.
    let tail_len = file_len.min((EOCD_LEN + 65_536 + 256) as u64);
    let tail_start = file_len - tail_len;
    f.seek(SeekFrom::Start(tail_start)).map_err(|_| not_zip())?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).map_err(|_| not_zip())?;

    let mut newest = true;
    let mut found = false;
    for pos in (0..tail.len().saturating_sub(EOCD_LEN - 1)).rev() {
        if tail.get(pos..pos + 4) != Some(&EOCD_SIG[..]) {
            continue;
        }
        let (Some(entries_disk), Some(entries), Some(cd_size), Some(cd_offset)) =
            (le16(&tail, pos + 8), le16(&tail, pos + 10), le32(&tail, pos + 12), le32(&tail, pos + 16))
        else {
            continue;
        };
        found = true;
        let is_newest = std::mem::replace(&mut newest, false);

        // Same decision as the crate: zip64 is used iff a field is saturated and a
        // zip64 locator directly precedes this record.
        let may_be_zip64 = entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF;
        let locator = pos
            .checked_sub(ZIP64_LOCATOR_LEN)
            .filter(|&loc| may_be_zip64 && tail.get(loc..loc + 4) == Some(&ZIP64_LOCATOR_SIG[..]));
        let Some(loc) = locator else {
            // Plain record: the 16-bit count is what the crate allocates for. Only
            // the record it tries first is authoritative (older ones may be stray
            // signatures and can never declare more than 65535 entries).
            if is_newest && entries.max(entries_disk) > max_entries {
                return Err(limit("too_many_entries"));
            }
            continue;
        };
        let (Some(loc_disk), Some(rec_off)) = (le32(&tail, loc + 4), le64(&tail, loc + 8)) else { continue };
        let loc_abs = tail_start + loc as u64;
        if rec_off >= loc_abs {
            continue; // the crate rejects this candidate outright
        }
        // Whatever zip64 record the crate ends up using lies before the locator and
        // satisfies `entries * 46 <= record offset`. If that alone keeps the count
        // within limits the layout does not matter; otherwise the record must be
        // exactly where the locator says and be the one the crate accepts without
        // searching for another signature, so that its count can be trusted.
        let must_pin = loc_abs / CD_RECORD_MIN > max_entries;
        let mut rec = [0u8; ZIP64_EOCD_LEN];
        let read_ok = f.seek(SeekFrom::Start(rec_off)).is_ok() && f.read_exact(&mut rec).is_ok();
        let fields = (
            le64(&rec, 4),
            le32(&rec, 16),
            le32(&rec, 20),
            le64(&rec, 24),
            le64(&rec, 32),
            le64(&rec, 40),
            le64(&rec, 48),
        );
        let (true, (Some(rec_size), Some(disk), Some(cd_disk), Some(n_disk), Some(n), Some(cd_size64), Some(cd_off64))) =
            (read_ok && rec.get(0..4) == Some(&ZIP64_EOCD_SIG[..]), fields)
        else {
            if must_pin {
                return Err(unsupported("zip64_layout"));
            }
            continue;
        };
        if n.max(n_disk) > max_entries {
            return Err(limit("too_many_entries"));
        }
        let fits = n.checked_mul(CD_RECORD_MIN).and_then(|v| v.checked_add(cd_off64)).is_some_and(|v| v <= rec_off)
            && cd_size64 <= file_len;
        let pinned =
            rec_size.checked_add(12) == Some(loc_abs - rec_off) && disk == 0 && cd_disk == loc_disk && n_disk == n && fits;
        if must_pin && !pinned {
            return Err(unsupported("zip64_layout"));
        }
    }
    if found {
        Ok(())
    } else {
        Err(not_zip())
    }
}

impl SafeZip {
    pub(crate) fn open(path: &Path, limits: &EpubLimits) -> Result<Self, AppError> {
        let mut file = File::open(path).map_err(|_| AppError::not_found("EPUB 파일"))?;
        let file_len = file.metadata().map_err(AppError::from)?.len();
        if file_len > limits.max_file_bytes {
            return Err(limit("file_too_large"));
        }
        check_declared_entries(&mut file, file_len, limits.max_entries)?;
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
