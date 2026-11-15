//! Secret storage for provider API keys and sign-in tokens (spec §6.2, T19): the macOS
//! Keychain, or DPAPI-encrypted files on Windows.
//!
//! service = `app.thoth.reader.provider`, account = the provider's `keychain_ref` (UUID).
//! There is deliberately no IPC that returns a stored key: the UI only learns
//! `keyConfigured` via [`has_secret`]. Reading the secret is crate-private and used only by
//! provider adapters right before a request. Error messages never contain secrets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{AppError, AppResult, ErrorCode};

pub const KEYCHAIN_SERVICE: &str = "app.thoth.reader.provider";
const MAX_SECRET_BYTES: usize = 8 * 1024;

/// A secret value. Debug/Display never reveal it; the inner string is crate-private and
/// overwritten on drop.
pub struct SecretValue(String);

impl SecretValue {
    pub(crate) fn new(s: String) -> Self {
        Self(s)
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue([redacted])")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        // Best-effort wipe of the heap buffer.
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

/// Storage backend for provider secrets.
pub trait SecretStore: Send + Sync {
    fn store(&self, account: &str, secret: &str) -> AppResult<()>;
    fn has(&self, account: &str) -> AppResult<bool>;
    fn delete(&self, account: &str) -> AppResult<()>;
    /// `Ok(None)` when no secret is stored. Only adapters call this (via [`SecretValue`]).
    fn read(&self, account: &str) -> AppResult<Option<SecretValue>>;
}

/// keychain_ref must be a UUID (lowercase hex + dashes).
pub fn validate_ref(r: &str) -> AppResult<()> {
    if uuid::Uuid::parse_str(r).is_err() || r.len() != 36 {
        return Err(AppError::invalid("키 참조 형식이 올바르지 않습니다."));
    }
    Ok(())
}

fn validate_secret(s: &str) -> AppResult<()> {
    if s.trim().is_empty() {
        return Err(AppError::invalid("API 키가 비어 있습니다."));
    }
    if s.len() > MAX_SECRET_BYTES || s.chars().any(|c| c.is_control()) {
        return Err(AppError::invalid("API 키 형식이 올바르지 않습니다."));
    }
    Ok(())
}

// OSStatus codes (Security.framework SecBase.h).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_SEC_AUTH_FAILED: i32 = -25293;
const ERR_SEC_USER_CANCELED: i32 = -128;
const ERR_SEC_NOT_AVAILABLE: i32 = -25291;
const ERR_SEC_NO_SUCH_KEYCHAIN: i32 = -25294;
const ERR_SEC_INTERACTION_REQUIRED: i32 = -25315;

/// Map an OSStatus to an AppError without leaking anything sensitive.
pub fn map_os_status(code: i32) -> AppError {
    match code {
        ERR_SEC_AUTH_FAILED | ERR_SEC_USER_CANCELED => AppError::new(
            ErrorCode::AuthFailed,
            "키체인 접근이 거부되었습니다. 접근을 허용하거나 API 키를 다시 입력해 주세요.",
        ),
        ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_INTERACTION_REQUIRED | ERR_SEC_NOT_AVAILABLE | ERR_SEC_NO_SUCH_KEYCHAIN => {
            AppError::new(
                ErrorCode::KeychainUnavailable,
                "키체인이 잠겨 있거나 사용할 수 없습니다. 잠금을 해제한 뒤 다시 시도해 주세요.",
            )
        }
        _ => AppError::new(ErrorCode::KeychainUnavailable, "키체인 작업에 실패했습니다. 다시 시도해 주세요."),
    }
    .with("osStatus", code)
}

/// The operating system's secret store (macOS Keychain, Windows DPAPI).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainStore;

#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn store(&self, account: &str, secret: &str) -> AppResult<()> {
        validate_ref(account)?;
        validate_secret(secret)?;
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, secret.as_bytes())
            .map_err(|e| map_os_status(e.code()))?;
        // Verify the write actually landed before reporting success (spec §6.2).
        if !self.has(account)? {
            return Err(AppError::new(ErrorCode::KeychainUnavailable, "키체인에 API 키를 저장하지 못했습니다."));
        }
        Ok(())
    }

    fn has(&self, account: &str) -> AppResult<bool> {
        Ok(self.read(account)?.is_some())
    }

    fn delete(&self, account: &str) -> AppResult<()> {
        validate_ref(account)?;
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(map_os_status(e.code())),
        }
    }

    fn read(&self, account: &str) -> AppResult<Option<SecretValue>> {
        validate_ref(account)?;
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes).map_err(|_| {
                    AppError::new(ErrorCode::KeychainUnavailable, "저장된 API 키를 읽을 수 없습니다. 키를 다시 입력해 주세요.")
                })?;
                Ok(Some(SecretValue::new(s)))
            }
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(map_os_status(e.code())),
        }
    }
}

/// Windows: secrets are encrypted with DPAPI (bound to the signed-in Windows user) and kept
/// as one file per provider under `%LOCALAPPDATA%\app.thoth.reader\secrets`. The Credential
/// Manager is not used because its entries are too small for sign-in tokens.
#[cfg(windows)]
mod dpapi {
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 }
    }

    /// Copies the output blob and frees the system allocation.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Option<Vec<u8>> {
        if out.pbData.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(v)
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        let input = blob(plain);
        let entropy = blob(super::KEYCHAIN_SERVICE.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: the input blobs point at live slices for the duration of the call; the
        // output blob is owned by the system until `take` copies and frees it.
        unsafe {
            if CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
                return None;
            }
            take(out)
        }
    }

    pub fn unprotect(cipher: &[u8]) -> Option<Vec<u8>> {
        let input = blob(cipher);
        let entropy = blob(super::KEYCHAIN_SERVICE.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as in `protect`.
        unsafe {
            if CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
                return None;
            }
            take(out)
        }
    }

    pub fn dir() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("app.thoth.reader").join("secrets"))
    }
}

#[cfg(windows)]
fn secret_path(account: &str) -> AppResult<std::path::PathBuf> {
    validate_ref(account)?;
    let dir = dpapi::dir().ok_or_else(|| map_os_status(ERR_SEC_NOT_AVAILABLE))?;
    Ok(dir.join(format!("{account}.bin")))
}

#[cfg(windows)]
impl SecretStore for KeychainStore {
    fn store(&self, account: &str, secret: &str) -> AppResult<()> {
        validate_secret(secret)?;
        let path = secret_path(account)?;
        let failed = || AppError::new(ErrorCode::KeychainUnavailable, "보안 저장소에 키를 저장하지 못했습니다.");
        let cipher = dpapi::protect(secret.as_bytes()).ok_or_else(failed)?;
        let dir = path.parent().ok_or_else(failed)?;
        std::fs::create_dir_all(dir).map_err(|_| failed())?;
        // write beside the target, then replace it, so a crash never leaves half a secret
        let tmp = dir.join(format!("{account}.tmp"));
        std::fs::write(&tmp, &cipher).map_err(|_| failed())?;
        std::fs::rename(&tmp, &path).map_err(|_| {
            let _ = std::fs::remove_file(&tmp);
            failed()
        })?;
        // Verify the write actually landed before reporting success (spec §6.2).
        if !self.has(account)? {
            return Err(failed());
        }
        Ok(())
    }

    fn has(&self, account: &str) -> AppResult<bool> {
        Ok(self.read(account)?.is_some())
    }

    fn delete(&self, account: &str) -> AppResult<()> {
        match std::fs::remove_file(secret_path(account)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AppError::new(ErrorCode::KeychainUnavailable, "보안 저장소에서 키를 지우지 못했습니다.")),
        }
    }

    fn read(&self, account: &str) -> AppResult<Option<SecretValue>> {
        let cipher = match std::fs::read(secret_path(account)?) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(map_os_status(ERR_SEC_NOT_AVAILABLE)),
        };
        // A file this Windows user cannot decrypt (copied from another account or machine)
        // is unusable: report it like an unreadable key so the user enters it again.
        let unreadable = || AppError::new(ErrorCode::KeychainUnavailable, "저장된 API 키를 읽을 수 없습니다. 키를 다시 입력해 주세요.");
        let plain = dpapi::unprotect(&cipher).ok_or_else(unreadable)?;
        Ok(Some(SecretValue::new(String::from_utf8(plain).map_err(|_| unreadable())?)))
    }
}

/// Other platforms have no secret store yet.
#[cfg(not(any(target_os = "macos", windows)))]
impl SecretStore for KeychainStore {
    fn store(&self, _: &str, _: &str) -> AppResult<()> {
        Err(map_os_status(ERR_SEC_NOT_AVAILABLE))
    }
    fn has(&self, _: &str) -> AppResult<bool> {
        Err(map_os_status(ERR_SEC_NOT_AVAILABLE))
    }
    fn delete(&self, _: &str) -> AppResult<()> {
        Err(map_os_status(ERR_SEC_NOT_AVAILABLE))
    }
    fn read(&self, _: &str) -> AppResult<Option<SecretValue>> {
        Err(map_os_status(ERR_SEC_NOT_AVAILABLE))
    }
}

/// In-memory store for tests (and for simulating a locked / denying Keychain, T19).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    items: Mutex<HashMap<String, String>>,
    /// When set, every operation fails with this OSStatus.
    fail_with: Mutex<Option<i32>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
    /// Simulate a locked keychain (`errSecInteractionNotAllowed`) or denial (`errSecAuthFailed`).
    pub fn fail_with_os_status(&self, code: Option<i32>) {
        *self.fail_with.lock().unwrap() = code;
    }
    pub fn set_locked(&self, locked: bool) {
        self.fail_with_os_status(locked.then_some(ERR_SEC_INTERACTION_NOT_ALLOWED));
    }
    pub fn set_denied(&self, denied: bool) {
        self.fail_with_os_status(denied.then_some(ERR_SEC_AUTH_FAILED));
    }
    fn check(&self) -> AppResult<()> {
        match *self.fail_with.lock().unwrap() {
            Some(code) => Err(map_os_status(code)),
            None => Ok(()),
        }
    }
}

impl SecretStore for MemorySecretStore {
    fn store(&self, account: &str, secret: &str) -> AppResult<()> {
        validate_ref(account)?;
        validate_secret(secret)?;
        self.check()?;
        self.items.lock().unwrap().insert(account.to_string(), secret.to_string());
        Ok(())
    }
    fn has(&self, account: &str) -> AppResult<bool> {
        validate_ref(account)?;
        self.check()?;
        Ok(self.items.lock().unwrap().contains_key(account))
    }
    fn delete(&self, account: &str) -> AppResult<()> {
        validate_ref(account)?;
        self.check()?;
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
    fn read(&self, account: &str) -> AppResult<Option<SecretValue>> {
        validate_ref(account)?;
        self.check()?;
        Ok(self.items.lock().unwrap().get(account).cloned().map(SecretValue::new))
    }
}

fn default_store() -> &'static Arc<dyn SecretStore> {
    static STORE: OnceLock<Arc<dyn SecretStore>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(KeychainStore))
}

/// The process-wide Keychain store (for `build_provider`).
pub fn keychain_store() -> Arc<dyn SecretStore> {
    default_store().clone()
}

/// Store (create or replace) the API key for `keychain_ref`. Fails if the Keychain write
/// could not be verified; callers must not mark the provider configured in that case.
pub fn store_secret(keychain_ref: &str, secret: &str) -> AppResult<()> {
    default_store().store(keychain_ref, secret)
}

/// Whether a key is stored (`keyConfigured`). Never returns the key.
pub fn has_secret(keychain_ref: &str) -> AppResult<bool> {
    default_store().has(keychain_ref)
}

/// Delete the key; deleting a missing key succeeds.
pub fn delete_secret(keychain_ref: &str) -> AppResult<()> {
    default_store().delete(keychain_ref)
}

/// Crate-private: read the key. Only provider adapters use this, right before a request.
#[allow(dead_code)]
pub(crate) fn read_secret(keychain_ref: &str) -> AppResult<Option<SecretValue>> {
    default_store().read(keychain_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: &str = "0192f6c4-8b7e-7cc3-9d2a-3f1e5b6a7c8d";

    #[test]
    fn memory_store_roundtrip_and_errors() {
        let s = MemorySecretStore::new();
        assert!(!s.has(REF).unwrap());
        s.store(REF, "sk-test-123").unwrap();
        assert!(s.has(REF).unwrap());
        assert_eq!(s.read(REF).unwrap().unwrap().expose(), "sk-test-123");
        let dbg = format!("{:?}", s.read(REF).unwrap().unwrap());
        assert!(!dbg.contains("sk-test"));
        s.delete(REF).unwrap();
        s.delete(REF).unwrap(); // idempotent
        assert!(s.read(REF).unwrap().is_none());

        assert!(s.store("not-a-uuid", "x").is_err());
        assert!(s.store(REF, "  ").is_err());
        assert!(s.store(REF, "a\nb").is_err());
    }

    #[test]
    fn locked_and_denied_map_without_secret() {
        let s = MemorySecretStore::new();
        s.set_locked(true);
        let e = s.store(REF, "sk-secret-value").unwrap_err();
        assert_eq!(e.code, ErrorCode::KeychainUnavailable);
        assert!(!format!("{e:?}").contains("sk-secret-value"));
        assert_eq!(s.read(REF).unwrap_err().code, ErrorCode::KeychainUnavailable);
        s.set_denied(true);
        assert_eq!(s.read(REF).unwrap_err().code, ErrorCode::AuthFailed);
        s.set_denied(false);
        assert!(s.read(REF).unwrap().is_none());
        assert_eq!(map_os_status(-1).code, ErrorCode::KeychainUnavailable);
        assert_eq!(map_os_status(ERR_SEC_USER_CANCELED).code, ErrorCode::AuthFailed);
    }

    /// Touches the real login Keychain; run manually: `cargo test --lib keychain -- --ignored`.
    #[test]
    #[ignore]
    fn real_keychain_roundtrip() {
        let r = uuid::Uuid::new_v4().to_string();
        store_secret(&r, "sk-thoth-test").unwrap();
        assert!(has_secret(&r).unwrap());
        assert_eq!(read_secret(&r).unwrap().unwrap().expose(), "sk-thoth-test");
        delete_secret(&r).unwrap();
        assert!(!has_secret(&r).unwrap());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn dpapi_store_round_trip() {
        let store = KeychainStore;
        let account = uuid::Uuid::new_v4().to_string();
        assert!(!store.has(&account).unwrap());
        assert!(store.read(&account).unwrap().is_none());
        store.store(&account, "sk-test-secret-값").unwrap();
        assert!(store.has(&account).unwrap());
        assert_eq!(store.read(&account).unwrap().unwrap().expose(), "sk-test-secret-값");
        // the file on disk is not the secret itself
        let on_disk = std::fs::read(secret_path(&account).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&on_disk).contains("sk-test-secret"));
        // replacing and deleting
        store.store(&account, "second").unwrap();
        assert_eq!(store.read(&account).unwrap().unwrap().expose(), "second");
        store.delete(&account).unwrap();
        store.delete(&account).unwrap();
        assert!(!store.has(&account).unwrap());
        // a corrupted file is reported, not treated as "no key"
        std::fs::create_dir_all(secret_path(&account).unwrap().parent().unwrap()).unwrap();
        std::fs::write(secret_path(&account).unwrap(), b"not a dpapi blob").unwrap();
        assert!(store.read(&account).is_err());
        store.delete(&account).unwrap();
        assert!(store.store("not-a-uuid", "x").is_err());
    }
}
