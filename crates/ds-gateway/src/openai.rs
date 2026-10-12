//! Format OpenAI-compat (port server/openai_format.py sums001).
//! DeepSeek tidak punya channel system/role — flatten messages jadi satu prompt.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Content,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
    Null,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub text: Option<String>,
}

pub fn text_of(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Null => String::new(),
        Content::Parts(ps) => ps
            .iter()
            .filter(|p| p.kind.as_deref() == Some("text"))
            .filter_map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn label(role: &str) -> &str {
    match role {
        "system" => "System",
        "user" => "User",
        "assistant" => "Assistant",
        _ => "User",
    }
}

/// Flatten riwayat OpenAI -> satu prompt DeepSeek.
pub fn messages_to_prompt(msgs: &[ChatMessage]) -> String {
    if msgs.len() == 1 && msgs[0].role == "user" {
        return text_of(&msgs[0].content);
    }
    let mut lines = vec![];
    for m in msgs {
        // tool -> sertakan sebagai System agar konteks tak hilang.
        let l = if m.role == "tool" { "Tool" } else { label(&m.role) };
        lines.push(format!("{l}: {}", text_of(&m.content)));
    }
    lines.push("Assistant:".into());
    lines.join("\n\n")
}

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    // Tanpa dep uuid: id acak dari waktu + counter.
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{:x}{:x}", now() as u64, n)
}

pub fn est_tokens(s: &str) -> i64 {
    std::cmp::max(1, s.len() as i64 / 4)
}

pub fn completion_response(
    model: &str,
    content: &str,
    prompt: &str,
    conversation_id: &str,
) -> serde_json::Value {
    completion_response_with_thinking(model, content, None, prompt, conversation_id)
}

/// Varian dengan reasoning (DeepSeek `reasoning_content`, ekstra non-OpenAI).
pub fn completion_response_with_thinking(
    model: &str,
    content: &str,
    thinking: Option<&str>,
    prompt: &str,
    conversation_id: &str,
) -> serde_json::Value {
    let pt = est_tokens(prompt);
    let ct = est_tokens(content);
    let mut message = serde_json::json!({"role": "assistant", "content": content});
    if let Some(t) = thinking {
        if !t.is_empty() {
            message["reasoning_content"] = serde_json::Value::String(t.to_string());
        }
    }
    serde_json::json!({
        "id": new_id(),
        "object": "chat.completion",
        "created": now(),
        "model": model,
        "conversation_id": conversation_id,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct},
    })
}

/// Bungkus stream delta DeepSeek -> SSE OpenAI (`data: {...}`, `data: [DONE]`).
pub fn sse_frame(model: &str, cid: &str, created: i64, delta: serde_json::Value, finish: Option<&str>, extra_cid: Option<&str>) -> String {
    let mut obj = serde_json::json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(x) = extra_cid {
        obj["conversation_id"] = serde_json::Value::String(x.to_string());
    }
    format!("data: {}\n\n", serde_json::to_string(&obj).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_user_verbatim() {
        let m = vec![ChatMessage { role: "user".into(), content: Content::Text("halo".into()) }];
        assert_eq!(messages_to_prompt(&m), "halo");
    }
    #[test]
    fn multi_turn_labels() {
        let m = vec![
            ChatMessage { role: "system".into(), content: Content::Text("jawab singkat".into()) },
            ChatMessage { role: "user".into(), content: Content::Text("hai".into()) },
        ];
        let p = messages_to_prompt(&m);
        assert!(p.contains("System: jawab singkat"));
        assert!(p.contains("User: hai"));
        assert!(p.ends_with("Assistant:"));
    }
    #[test]
    fn parts_extract() {
        let m = ChatMessage {
            role: "user".into(),
            content: Content::Parts(vec![
                ContentPart { kind: Some("text".into()), text: Some("a".into()) },
                ContentPart { kind: Some("image_url".into()), text: None },
            ]),
        };
        assert_eq!(text_of(&m.content), "a");
    }
}
