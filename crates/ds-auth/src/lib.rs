//! ds-auth: 1 akun DeepSeek = 1 DsSocket (tiru WaSocket, bukan manager global).
//!
//! Fakta riset F0 (jujur): alur email+password murni HTTP sudah mati
//! (coder.deepseek.com lama). Situs modern memakai AWS WAF + bearer token
//! `localStorage.userToken` + cookies + PoW per pesan. Jadi login utama adalah
//! **import token** (paste dari DevTools / hasil `python -m deepseek.auth`),
//! bukan password. Metode password tetap dicoba (endpoint legacy
//! `/api/v0/users/login`) tapi kegagalan-nya memberi hint manual yang jelas.

pub mod socket;
pub use socket::{DsConfig, DsSocket};
