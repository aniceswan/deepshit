//! Konstanta protokol chat.deepseek.com (hasil riset F0, lihat deepseek-protocol.md).

/// Basis web chat (bukan api.deepseek.com berbayar).
pub const BASE_URL: &str = "https://chat.deepseek.com";
pub const API_PREFIX: &str = "/api/v0";

pub const PATH_CREATE_SESSION: &str = "/api/v0/chat_session/create";
pub const PATH_FETCH_SESSIONS: &str = "/api/v0/chat_session/fetch_page";
pub const PATH_DELETE_SESSION: &str = "/api/v0/chat_session/delete";
pub const PATH_UPDATE_TITLE: &str = "/api/v0/chat_session/update_title";
pub const PATH_POW_CHALLENGE: &str = "/api/v0/chat/create_pow_challenge";
pub const PATH_COMPLETION: &str = "/api/v0/chat/completion";
pub const PATH_STOP_STREAM: &str = "/api/v0/chat/stop_stream";
pub const PATH_HISTORY: &str = "/api/v0/chat/history_messages";
pub const PATH_UPLOAD: &str = "/api/v0/file/upload_file";
pub const PATH_FETCH_FILES: &str = "/api/v0/file/fetch_files";
pub const PATH_LOGIN: &str = "/api/v0/users/login";
pub const PATH_CURRENT_USER: &str = "/api/v0/users/current";

/// Header PoW wajib untuk completion + upload.
pub const HDR_POW: &str = "x-ds-pow-response";

/// Default header klien web (meniru browser; penting untuk WAF).
pub const X_APP_VERSION: &str = "2.0.0";
pub const X_CLIENT_VERSION: &str = "2.0.0";
pub const X_CLIENT_PLATFORM: &str = "web";
pub const X_CLIENT_LOCALE: &str = "en_US";

/// Model publik (lapisan OpenAI) -> model_type wire DeepSeek.
/// DeepThink/search BUKAN model — itu flag orthogonal per request.
pub const MODEL_CHAT: &str = "deepseek-chat";
pub const MODEL_REASONER: &str = "deepseek-reasoner";
/// Alias yang diterima gateway (dipetakan ke dua di atas).
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("deepseek-chat", "default"),
    ("deepseek-v3", "default"),
    ("deepseek-v4", "default"),
    ("deepseek-expert", "expert"),
    ("deepseek-reasoner", "expert"),
    ("deepseek-r1", "expert"),
    ("deepseek-chat-search", "default"),
    ("deepseek-reasoner-search", "expert"),
    ("deepseek-r1-search", "expert"),
    ("deepseek-v3-search", "default"),
];

/// Batas PoW tipikal dari server.
pub const POW_DEFAULT_DIFFICULTY: u64 = 144_000;

/// Umur session file sebelum refresh headless (6 jam, meniru sums001).
pub const SESSION_MAX_AGE_SECS: u64 = 6 * 60 * 60;

/// Rate-limit default gateway untuk /v1 (per IP, per menit).
pub const RATE_LIMIT_PER_MINUTE: u32 = 30;

/// Sufiks conversation_id: "<chat_session_id>:<parent_message_id>".
pub const CID_SEP: char = ':';
