//! Tipe publik: session, chat, pesan, PoW, balasan.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::constants::CID_SEP;

/// Session tersimpan per akun: token + cookies + UA (hasil capture browser).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub cookies: HashMap<String, String>,
    pub user_agent: String,
    pub captured_at: u64,
}

impl Session {
    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.captured_at)
    }
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

pub fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Opsi kirim (flag orthogonal — bukan bagian nama model).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SendOpt {
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub search: bool,
    /// "default" (Instant) atau "expert"; None = biarkan thread lama.
    #[serde(default)]
    pub model_type: Option<ModelType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    Default,
    Expert,
}

impl ModelType {
    pub fn as_wire(&self) -> &'static str {
        match self {
            ModelType::Default => "default",
            ModelType::Expert => "expert",
        }
    }
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "default" => Some(ModelType::Default),
            "expert" => Some(ModelType::Expert),
            _ => None,
        }
    }
}

/// Balasan lengkap + thinking opsional + id untuk melanjutkan thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    pub conversation_id: String,
}

/// Info chat session (daftar riwayat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatInfo {
    pub id: String,
    pub title: Option<String>,
    pub created_at: Option<u64>,
}

/// Satu pesan riwayat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String,
    pub content: String,
    pub message_id: Option<i64>,
}

/// Challenge PoW dari server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: u64,
    pub expire_at: i64,
    pub target_path: String,
}

impl Challenge {
    pub fn prefix(&self) -> String {
        format!("{}_{}_", self.salt, self.expire_at)
    }
}

/// Encode/decode conversation_id "<session>[:<msg_id>]".
pub fn encode_cid(session_id: &str, message_id: Option<i64>) -> String {
    match message_id {
        Some(m) => format!("{session_id}{CID_SEP}{m}"),
        None => session_id.to_string(),
    }
}

pub fn decode_cid(cid: Option<&str>) -> (Option<String>, Option<i64>) {
    let cid = match cid {
        Some(s) if !s.is_empty() => s,
        _ => return (None, None),
    };
    let mut it = cid.splitn(2, CID_SEP);
    let sid = it.next().unwrap_or("").to_string();
    let msg = it.next().and_then(|m| m.parse::<i64>().ok());
    if sid.is_empty() {
        (None, None)
    } else {
        (Some(sid), msg)
    }
}

/// Petakan nama model publik -> model_type wire. Unknown -> None.
pub fn resolve_model_type(name: &str) -> Option<ModelType> {
    let lower = name.to_lowercase();
    for (alias, wire) in crate::constants::MODEL_ALIASES {
        if *alias == lower {
            return ModelType::from_wire(wire);
        }
    }
    None
}

pub fn is_known_model(name: &str) -> bool {
    resolve_model_type(name).is_some()
}

pub fn known_models() -> Vec<&'static str> {
    vec!["deepseek-chat", "deepseek-reasoner"]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cid_roundtrip() {
        assert_eq!(encode_cid("abc", None), "abc");
        assert_eq!(encode_cid("abc", Some(12)), "abc:12");
        assert_eq!(decode_cid(None), (None, None));
        assert_eq!(decode_cid(Some("")), (None, None));
        assert_eq!(
            decode_cid(Some("abc:12")),
            (Some("abc".into()), Some(12))
        );
        assert_eq!(decode_cid(Some("abc")), (Some("abc".into()), None));
        assert_eq!(decode_cid(Some("abc:xx")), (Some("abc".into()), None));
    }
    #[test]
    fn model_map() {
        assert!(is_known_model("deepseek-chat"));
        assert!(is_known_model("deepseek-reasoner"));
        assert!(is_known_model("deepseek-r1"));
        assert!(is_known_model("DEEPSEEK-CHAT"));
        assert!(!is_known_model("gpt-4"));
        assert_eq!(resolve_model_type("deepseek-chat"), Some(ModelType::Default));
        assert_eq!(resolve_model_type("deepseek-reasoner"), Some(ModelType::Expert));
    }
}
