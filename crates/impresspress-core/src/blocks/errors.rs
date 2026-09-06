/// Standardized error codes for impresspress API responses.
/// Used in place of string-based error matching for reliable error handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Auth errors
    InvalidCredentials,
    EmailAlreadyExists,
    AccountDisabled,
    NotAuthenticated,
    InvalidToken,
    TokenExpired,
    EmailNotVerified,
    PasswordTooShort,
    PasswordTooLong,
    InvalidEmail,
    InvalidInput,

    // Authorization
    Forbidden,
    AdminRequired,

    // Resource errors
    NotFound,
    Conflict,

    // Database
    DatabaseError,

    // Payment
    PaymentNotConfigured,
    InvalidPurchaseStatus,
    RefundFailed,

    // Storage
    QuotaExceeded,
    FileTooLarge,

    // System
    InternalError,
    ConfigurationError,
    RateLimitExceeded,
}

impl ErrorCode {
    /// Stable machine-readable identifier (e.g. `"invalid_credentials"`).
    /// Surfaced in JSON error responses as the `code` field; callers should
    /// switch on this rather than parsing the human-readable message.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid_credentials",
            Self::EmailAlreadyExists => "email_already_exists",
            Self::AccountDisabled => "account_disabled",
            Self::NotAuthenticated => "not_authenticated",
            Self::InvalidToken => "invalid_token",
            Self::TokenExpired => "token_expired",
            Self::EmailNotVerified => "email_not_verified",
            Self::PasswordTooShort => "password_too_short",
            Self::PasswordTooLong => "password_too_long",
            Self::InvalidEmail => "invalid_email",
            Self::InvalidInput => "invalid_input",
            Self::Forbidden => "forbidden",
            Self::AdminRequired => "admin_required",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::DatabaseError => "database_error",
            Self::PaymentNotConfigured => "payment_not_configured",
            Self::InvalidPurchaseStatus => "invalid_purchase_status",
            Self::RefundFailed => "refund_failed",
            Self::QuotaExceeded => "quota_exceeded",
            Self::FileTooLarge => "file_too_large",
            Self::InternalError => "internal_error",
            Self::ConfigurationError => "configuration_error",
            Self::RateLimitExceeded => "rate_limit_exceeded",
        }
    }

    /// The human half of this code, for the sites that have nothing more
    /// specific to say. [`error_response`] takes a message because most
    /// callers do have something specific; this is what
    /// `From<ErrorCode> for OutputStream` uses when the code IS the whole
    /// story.
    ///
    /// Never a repeat of [`as_str`](Self::as_str): the machine-readable code
    /// travels as `error.code` meta, and a message that restates it tells a
    /// human nothing.
    pub fn default_message(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "Invalid email or password",
            Self::EmailAlreadyExists => "That email is already registered",
            Self::AccountDisabled => "This account is disabled",
            Self::NotAuthenticated => "Not authenticated",
            Self::InvalidToken => "Invalid token",
            Self::TokenExpired => "Token expired",
            Self::EmailNotVerified => "Email address not verified",
            Self::PasswordTooShort => "Password is too short",
            Self::PasswordTooLong => "Password is too long",
            Self::InvalidEmail => "Invalid email address",
            Self::InvalidInput => "Invalid input",
            Self::Forbidden => "Access denied",
            Self::AdminRequired => "Administrator access required",
            Self::NotFound => "Not found",
            Self::Conflict => "Conflicts with the current state",
            Self::DatabaseError => "Database error",
            Self::PaymentNotConfigured => "Payments are not configured",
            Self::InvalidPurchaseStatus => "The purchase is not in a state that allows this",
            Self::RefundFailed => "The refund could not be completed",
            Self::QuotaExceeded => "Storage quota exceeded",
            Self::FileTooLarge => "File is too large",
            Self::InternalError => "Internal server error",
            Self::ConfigurationError => "Configuration error",
            Self::RateLimitExceeded => "Too many requests — try again later",
        }
    }

    /// Every variant, so a test can walk the set and a new variant added
    /// without a message here fails to compile rather than shipping one.
    pub const ALL: [Self; 24] = [
        Self::InvalidCredentials,
        Self::EmailAlreadyExists,
        Self::AccountDisabled,
        Self::NotAuthenticated,
        Self::InvalidToken,
        Self::TokenExpired,
        Self::EmailNotVerified,
        Self::PasswordTooShort,
        Self::PasswordTooLong,
        Self::InvalidEmail,
        Self::InvalidInput,
        Self::Forbidden,
        Self::AdminRequired,
        Self::NotFound,
        Self::Conflict,
        Self::DatabaseError,
        Self::PaymentNotConfigured,
        Self::InvalidPurchaseStatus,
        Self::RefundFailed,
        Self::QuotaExceeded,
        Self::FileTooLarge,
        Self::InternalError,
        Self::ConfigurationError,
        Self::RateLimitExceeded,
    ];
}

/// The refusal an [`ErrorCode`] is, when the code is the whole story.
///
/// This is the one `From` impl the orphan rules allow — `From<LocalType> for
/// OutputStream` — and it is what lets a match arm read
/// `ErrorCode::NotFound.into()` instead of spelling out
/// `error_response(ErrorCode::NotFound, "Not found")`. Callers that have
/// something more specific to say keep using [`error_response`].
impl From<ErrorCode> for wafer_run::OutputStream {
    fn from(code: ErrorCode) -> Self {
        error_response(code, code.default_message())
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Helper to create a JSON error response with a structured error code.
///
/// Maps the fine-grained impresspress [`ErrorCode`] to the coarse wafer
/// `ErrorCode` (which drives transport/status mapping) and attaches the
/// precise impresspress code as structured `error.code` meta via
/// [`wafer_run::WaferError::with_detail_code`] — the
/// `wafer_block::META_ERROR_CODE` convention. The message stays human-only;
/// the old `"[{code}] {message}"` in-band prefix is gone, so HTTP adapters
/// surface the machine-readable code as a JSON `code` field from the meta
/// rather than callers parsing it back out of the message.
pub fn error_response(code: ErrorCode, message: &str) -> wafer_run::OutputStream {
    let wafer_code = impresspress_error_code_to_wafer(code);
    wafer_run::OutputStream::error(
        wafer_run::WaferError::new(wafer_code, message.to_string()).with_detail_code(code.as_str()),
    )
}

/// Map a impresspress `ErrorCode` to a wafer `ErrorCode`.
///
/// This is the ONLY thing that decides an [`ErrorCode`]'s HTTP status, and it
/// decides it indirectly: the wafer code is what
/// `wafer_block::http_codec::error_code_to_http_status` reads, through
/// `resolve_error_status`, at the HTTP boundary. There is deliberately no
/// second status table beside this one — the `ErrorCode::status_code()` that
/// used to sit here had zero callers and disagreed with this mapping
/// (`QuotaExceeded` → 413, where `ResourceExhausted` → 429 actually ships).
pub(crate) fn impresspress_error_code_to_wafer(code: ErrorCode) -> wafer_run::ErrorCode {
    match code {
        ErrorCode::InvalidCredentials
        | ErrorCode::NotAuthenticated
        | ErrorCode::InvalidToken
        | ErrorCode::TokenExpired => wafer_run::ErrorCode::Unauthenticated,

        ErrorCode::Forbidden
        | ErrorCode::AdminRequired
        | ErrorCode::AccountDisabled
        | ErrorCode::EmailNotVerified => wafer_run::ErrorCode::PermissionDenied,

        ErrorCode::NotFound => wafer_run::ErrorCode::NotFound,

        ErrorCode::EmailAlreadyExists | ErrorCode::Conflict => wafer_run::ErrorCode::AlreadyExists,

        ErrorCode::PasswordTooShort
        | ErrorCode::PasswordTooLong
        | ErrorCode::InvalidEmail
        | ErrorCode::InvalidInput
        | ErrorCode::InvalidPurchaseStatus => wafer_run::ErrorCode::InvalidArgument,

        ErrorCode::QuotaExceeded | ErrorCode::FileTooLarge => {
            wafer_run::ErrorCode::ResourceExhausted
        }

        ErrorCode::RateLimitExceeded => wafer_run::ErrorCode::ResourceExhausted,

        ErrorCode::PaymentNotConfigured
        | ErrorCode::ConfigurationError
        | ErrorCode::DatabaseError
        | ErrorCode::InternalError
        | ErrorCode::RefundFailed => wafer_run::ErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status an `ErrorCode` actually ships as, read from the response
    /// rather than from a second table beside the mapping.
    ///
    /// This replaces `test_error_code_status_codes`, which asserted a
    /// `status_code()` method with zero callers and one wrong answer: it said
    /// `QuotaExceeded | FileTooLarge => 413` while both map to
    /// `wafer_run::ErrorCode::ResourceExhausted`, which
    /// `http_codec::error_code_to_http_status` renders as **429**. Asking the
    /// response is the only assertion that cannot drift from what a client
    /// receives.
    async fn shipped_status(code: ErrorCode) -> u16 {
        match error_response(code, "message").collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(e)) => {
                wafer_block::http_codec::resolve_error_status(&e)
            }
            other => panic!("expected an error stream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn quota_ships_as_429_not_the_413_the_deleted_table_claimed() {
        assert_eq!(shipped_status(ErrorCode::QuotaExceeded).await, 429);
        assert_eq!(shipped_status(ErrorCode::FileTooLarge).await, 429);
        assert_eq!(shipped_status(ErrorCode::RateLimitExceeded).await, 429);
    }

    #[tokio::test]
    async fn every_other_class_ships_the_status_its_wafer_code_maps_to() {
        assert_eq!(shipped_status(ErrorCode::InvalidCredentials).await, 401);
        assert_eq!(shipped_status(ErrorCode::TokenExpired).await, 401);
        assert_eq!(shipped_status(ErrorCode::Forbidden).await, 403);
        assert_eq!(shipped_status(ErrorCode::AccountDisabled).await, 403);
        assert_eq!(shipped_status(ErrorCode::NotFound).await, 404);
        assert_eq!(shipped_status(ErrorCode::Conflict).await, 409);
        assert_eq!(shipped_status(ErrorCode::EmailAlreadyExists).await, 409);
        assert_eq!(shipped_status(ErrorCode::InvalidInput).await, 400);
        assert_eq!(shipped_status(ErrorCode::PasswordTooShort).await, 400);
        assert_eq!(shipped_status(ErrorCode::DatabaseError).await, 500);
        assert_eq!(shipped_status(ErrorCode::InternalError).await, 500);
        assert_eq!(shipped_status(ErrorCode::ConfigurationError).await, 500);
    }

    #[tokio::test]
    async fn an_error_code_converts_into_a_response_carrying_its_default_message() {
        let out: wafer_run::OutputStream = ErrorCode::NotFound.into();
        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(err)) => {
                assert_eq!(err.code, wafer_run::ErrorCode::NotFound);
                assert_eq!(err.detail_code(), Some("not_found"));
                assert_eq!(err.message, ErrorCode::NotFound.default_message());
                assert!(!err.message.is_empty());
            }
            other => panic!("expected an error stream, got {other:?}"),
        }
    }

    /// Every variant has a message; none of them is the machine code
    /// leaking into the human half.
    #[test]
    fn no_default_message_is_empty_or_a_repeat_of_the_code() {
        for code in ErrorCode::ALL {
            let message = code.default_message();
            assert!(!message.is_empty(), "{code} has no default message");
            assert_ne!(
                message,
                code.as_str(),
                "{code}'s default message is its machine code"
            );
        }
    }

    #[test]
    fn test_error_code_as_str() {
        assert_eq!(
            ErrorCode::InvalidCredentials.as_str(),
            "invalid_credentials"
        );
        assert_eq!(
            ErrorCode::EmailAlreadyExists.as_str(),
            "email_already_exists"
        );
        assert_eq!(ErrorCode::RateLimitExceeded.as_str(), "rate_limit_exceeded");
        assert_eq!(ErrorCode::QuotaExceeded.as_str(), "quota_exceeded");
    }

    #[test]
    fn test_error_code_display() {
        assert_eq!(format!("{}", ErrorCode::NotFound), "not_found");
        assert_eq!(format!("{}", ErrorCode::InvalidToken), "invalid_token");
    }

    #[tokio::test]
    async fn error_response_carries_code_as_structured_meta() {
        // The precise impresspress code lands in `error.code` meta (not as a
        // `"[code] "` prefix on the human message), and the coarse wafer code
        // is the transport classification.
        let out = error_response(ErrorCode::InvalidToken, "token is bad");
        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(err)) => {
                assert_eq!(err.code, wafer_run::ErrorCode::Unauthenticated);
                assert_eq!(err.message, "token is bad");
                assert!(
                    !err.message.starts_with('['),
                    "message must not carry the old bracket-code prefix"
                );
                assert_eq!(err.detail_code(), Some("invalid_token"));
            }
            other => panic!("expected an error stream, got {other:?}"),
        }
    }
}
