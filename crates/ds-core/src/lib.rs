//! ds-core: tipe publik + error + konstanta + PoW + parser SSE.
//! Port pola wa-core (Types/Defaults) untuk protokol chat.deepseek.com.

pub mod constants;
pub mod error;
pub mod pow;
pub mod sse;
pub mod types;

pub use constants::*;
pub use error::DsError;
pub use pow::{build_pow_header, solve_pow, deepseek_hash_v1, deepseek_hash_hex, build_prefix, solve_and_build_header};
pub use sse::{SseEvent, SseParser};
pub use types::*;
