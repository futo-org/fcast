use thiserror::Error;

#[derive(Debug, Error)]
pub enum SabrError {
    /// The server rejected the (static) PoToken / attestation. Fatal for this
    /// session. The caller must obtain a fresh player response and PoToken.
    #[error("SABR blocked: {0}")]
    Blocked(String),

    /// The server asked for a fresh player response (RELOAD_PLAYER_RESPONSE).
    #[error("SABR reload required: {0}")]
    ReloadRequired(String),

    /// The server persistently served a different format than requested,
    /// indicating the caller's format list is out of sync.
    #[error("SABR format substituted: {0}")]
    FormatSubstituted(String),

    /// A SABR_ERROR part, or another protocol-level failure.
    #[error("SABR protocol error: {0}")]
    Protocol(String),

    /// An HTTP-level failure (non-2xx or transport error).
    #[error("SABR HTTP error: {0}")]
    Http(String),

    /// An I/O error reading the UMP stream.
    #[error("SABR I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A protobuf decode failure.
    #[error("SABR decode error: {0}")]
    Decode(#[from] prost::DecodeError),
}

impl SabrError {
    /// Whether this error is fatal to the session (no point retrying).
    pub fn is_fatal(&self) -> bool {
        matches!(self, SabrError::Blocked(_) | SabrError::ReloadRequired(_))
    }
}

pub type SabrResult<T> = Result<T, SabrError>;
