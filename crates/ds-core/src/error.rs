use thiserror::Error;

#[derive(Debug, Error)]
pub enum DsError {
    #[error("not authenticated: {0}")]
    NotAuthenticated(String),
    #[error("login required: {0}")]
    LoginRequired(String),
    #[error("protocol error {code}: {text}")]
    Protocol { code: u16, text: String },
    #[error("api error: {0}")]
    Api(String),
    #[error("pow failed: {0}")]
    Pow(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("store: {0}")]
    Store(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("rate limited, retry after {0:.1}s")]
    RateLimited(f64),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
