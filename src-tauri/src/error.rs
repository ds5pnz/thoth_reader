//! Application error codes (spec §8.4). Messages describe the next user action and
//! never contain source text, keys or full local paths.

use serde::Serialize;
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export)]
pub enum ErrorCode {
    InvalidArgument,
    UnsupportedFormat,
    LimitExceeded,
    PasswordRequired,
    DrmUnsupported,
    ExtractionPartial,
    StaleSession,
    Conflict,
    ProviderUnavailable,
    AuthFailed,
    RateLimited,
    ContextTooLarge,
    InvalidOutput,
    Cancelled,
    DbFull,
    MigrationFailed,
    NotFound,
    PermissionDenied,
    KeychainUnavailable,
    Internal,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::LimitExceeded => "LIMIT_EXCEEDED",
            Self::PasswordRequired => "PASSWORD_REQUIRED",
            Self::DrmUnsupported => "DRM_UNSUPPORTED",
            Self::ExtractionPartial => "EXTRACTION_PARTIAL",
            Self::StaleSession => "STALE_SESSION",
            Self::Conflict => "CONFLICT",
            Self::ProviderUnavailable => "PROVIDER_UNAVAILABLE",
            Self::AuthFailed => "AUTH_FAILED",
            Self::RateLimited => "RATE_LIMITED",
            Self::ContextTooLarge => "CONTEXT_TOO_LARGE",
            Self::InvalidOutput => "INVALID_OUTPUT",
            Self::Cancelled => "CANCELLED",
            Self::DbFull => "DB_FULL",
            Self::MigrationFailed => "MIGRATION_FAILED",
            Self::NotFound => "NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::KeychainUnavailable => "KEYCHAIN_UNAVAILABLE",
            Self::Internal => "INTERNAL",
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ProviderUnavailable | Self::RateLimited | Self::DbFull | Self::KeychainUnavailable
        )
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    pub details: BTreeMap<String, serde_json::Value>,
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), details: BTreeMap::new() }
    }
    pub fn with(mut self, k: &str, v: impl Into<serde_json::Value>) -> Self {
        self.details.insert(k.to_string(), v.into());
        self
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, msg)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what}을(를) 찾을 수 없습니다."))
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, msg)
    }
    pub fn stale() -> Self {
        Self::new(ErrorCode::StaleSession, "문서 세션이 만료되었습니다. 문서를 다시 열어 주세요.")
    }
    pub fn limit(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::LimitExceeded, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, msg)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        let s = e.to_string();
        if s.contains("database or disk is full") || s.contains("SQLITE_FULL") {
            return AppError::new(ErrorCode::DbFull, "디스크 공간이 부족합니다. 공간을 확보한 뒤 다시 시도해 주세요.");
        }
        tracing::error!(target: "db", "sqlx error: {}", crate::diagnostics::redact(&s));
        AppError::internal("데이터베이스 작업에 실패했습니다.")
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        if e.raw_os_error() == Some(28) {
            return AppError::new(ErrorCode::DbFull, "디스크 공간이 부족합니다.");
        }
        tracing::error!(target: "io", "io error kind={:?}", e.kind());
        AppError::internal("파일 작업에 실패했습니다.")
    }
}

impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        AppError::invalid("잘못된 데이터 형식입니다.")
    }
}

/// Wire shape of `CommandResult.error`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct WireError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[ts(optional, type = "Record<string, string | number | boolean>")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, serde_json::Value>,
}

impl From<&AppError> for WireError {
    fn from(e: &AppError) -> Self {
        WireError {
            code: e.code.as_str().to_string(),
            message: e.message.clone(),
            retryable: e.code.retryable(),
            details: e.details.clone(),
        }
    }
}
