//! Parser SSE completion DeepSeek -> delta teks.
//!
//! Bentuk stream (hasil riset F0, port _parse_sse Python sums001):
//! - frame snapshot: {"v": {"response": {"fragments": [{"type":"RESPONSE","content":"..."}]}}}
//! - frame penentu path: {"p":"response/fragments/-1/content","o":"APPEND","v":"..."}
//! - frame bare: {"v":" lanjutan"} (append ke path aktif)
//! - selesai: {"p":"response/status","v":"FINISHED"} atau data: [DONE]
//!
//! Kita hanya emisikan teks fragment bertipe RESPONSE.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    Text(String),
    Thinking(String),
    MessageId(i64),
    Finished,
    Ignored,
}

/// Parser stateful: ingat append-path + mode thinking antar baris.
#[derive(Debug, Default)]
pub struct SseParser {
    active_path: Option<String>,
    emitted_initial: bool,
    in_thinking: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Umpan satu baris SSE mentah ("data: {...}"). Mengembalikan 0..n event.
    pub fn feed_line(&mut self, line: &str) -> Vec<SseEvent> {
        parse_sse_line(
            line,
            &mut self.active_path,
            &mut self.emitted_initial,
            &mut self.in_thinking,
        )
        .into_iter()
        .filter(|ev| *ev != SseEvent::Ignored)
        .collect()
    }

    /// Umpan banyak baris; kumpulkan teks saja (nyaman untuk non-stream).
    /// Thinking diabaikan (pakai `collect_full` bila butuh).
    pub fn collect_text(lines: impl IntoIterator<Item = String>) -> (String, Option<i64>) {
        let (t, _, m) = Self::collect_full(lines);
        (t, m)
    }

    /// Kumpulkan (jawaban, thinking, message_id).
    pub fn collect_full(
        lines: impl IntoIterator<Item = String>,
    ) -> (String, Option<String>, Option<i64>) {
        let mut p = SseParser::new();
        let mut out = String::new();
        let mut think = String::new();
        let mut mid = None;
        for l in lines {
            for ev in p.feed_line(&l) {
                match ev {
                    SseEvent::Text(t) => out.push_str(&t),
                    SseEvent::Thinking(t) => think.push_str(&t),
                    SseEvent::MessageId(m) => mid = Some(m),
                    _ => {}
                }
            }
        }
        (out, if think.is_empty() { None } else { Some(think) }, mid)
    }
}

fn parse_sse_line(
    line: &str,
    active_path: &mut Option<String>,
    emitted_initial: &mut bool,
    in_thinking: &mut bool,
) -> Vec<SseEvent> {
    let t = line.trim();
    if t.is_empty() || !t.starts_with("data:") {
        return vec![];
    }
    let payload = t["data:".len()..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return vec![];
    }
    let obj: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    // Snapshot: v.response — bisa memuat message_id DAN fragments sekaligus.
    // Kembalikan keduanya (dulu bug: return dini saat message_id ada).
    if let Some(v) = obj.get("v") {
        if v.is_object() && v.get("response").is_some() {
            let resp = &v["response"];
            let mut out = vec![];
            if let Some(mid) = resp
                .get("message_id")
                .or_else(|| resp.get("id"))
                .and_then(|x| x.as_i64())
            {
                out.push(SseEvent::MessageId(mid));
            }
            if let Some(frags) = resp.get("fragments").and_then(|x| x.as_array()) {
                for f in frags {
                    let ftype = f.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    let is_think = ftype == "THINK" || ftype == "THINKING";
                    let is_resp = ftype == "RESPONSE";
                    if let Some(mid) = f.get("message_id").and_then(|x| x.as_i64()) {
                        out.push(SseEvent::MessageId(mid));
                    }
                    if is_think {
                        *in_thinking = true;
                        if let Some(c) = f.get("content").and_then(|x| x.as_str()) {
                            if !c.is_empty() {
                                *active_path = Some("response/fragments/-1/content".into());
                                out.push(SseEvent::Thinking(c.to_string()));
                            }
                        }
                    } else if is_resp {
                        *in_thinking = false;
                        if let Some(c) = f.get("content").and_then(|x| x.as_str()) {
                            if !c.is_empty() {
                                *active_path = Some("response/fragments/-1/content".into());
                                if !*emitted_initial {
                                    *emitted_initial = true;
                                    out.push(SseEvent::Text(c.to_string()));
                                }
                            }
                        }
                    }
                }
            }
            if out.is_empty() {
                out.push(SseEvent::Ignored);
            }
            return out;
        }
    }
    // Frame dengan path eksplisit
    if let Some(p) = obj.get("p").and_then(|x| x.as_str()) {
        // Jawaban final thinking: {"p":"response/fragments","o":"APPEND","v":[{type,content}]}
        if p == "response/fragments"
            && obj.get("o").and_then(|x| x.as_str()) == Some("APPEND")
        {
            if let Some(arr) = obj.get("v").and_then(|x| x.as_array()) {
                let mut out = vec![];
                for f in arr {
                    let ftype = f.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    let content = f.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    if content.is_empty() {
                        continue;
                    }
                    if ftype == "THINK" || ftype == "THINKING" {
                        *in_thinking = true;
                        out.push(SseEvent::Thinking(content.to_string()));
                    } else {
                        *in_thinking = false;
                        out.push(SseEvent::Text(content.to_string()));
                    }
                }
                if out.is_empty() {
                    out.push(SseEvent::Ignored);
                }
                return out;
            }
        }
        *active_path = Some(p.to_string());
        if let Some(v) = obj.get("v") {
            if v.is_number() && p.ends_with("message_id") {
                if let Some(m) = v.as_i64() {
                    return vec![SseEvent::MessageId(m)];
                }
            }
            if p == "response/status" && v.as_str() == Some("FINISHED") {
                *in_thinking = false;
                return vec![SseEvent::Finished];
            }
            // elapsed_secs / path SET lain: bukan konten.
            if p.ends_with("elapsed_secs") {
                return vec![SseEvent::Ignored];
            }
            if obj.get("o").and_then(|x| x.as_str()) == Some("APPEND")
                && p.ends_with("content")
            {
                if let Some(s) = v.as_str() {
                    if *in_thinking {
                        return vec![SseEvent::Thinking(s.to_string())];
                    }
                    return vec![SseEvent::Text(s.to_string())];
                }
            }
        }
        return vec![SseEvent::Ignored];
    }
    // Bare append: {"v":"..."} tanpa p
    if let Some(v) = obj.get("v") {
        if let Some(s) = v.as_str() {
            if let Some(ap) = active_path {
                if ap.ends_with("content") {
                    if *in_thinking {
                        return vec![SseEvent::Thinking(s.to_string())];
                    }
                    return vec![SseEvent::Text(s.to_string())];
                }
            }
        }
        // v non-string (mis. angka) tanpa path -> abaikan
        return vec![SseEvent::Ignored];
    }
    // Bentuk lama dskpp: request_message_id/response_message_id
    if obj.get("request_message_id").is_some() {
        if let Some(m) = obj.get("response_message_id").and_then(|x| x.as_i64()) {
            return vec![SseEvent::MessageId(m)];
        }
        return vec![SseEvent::Ignored];
    }
    // Bentuk update_session/title/close (event: ...) — abaikan.
    vec![SseEvent::Ignored]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_then_appends() {
        let mut p = SseParser::new();
        let lines = [
            r#"data: {"v":{"response":{"fragments":[{"type":"RESPONSE","content":"Halo"}]}}}"#,
            r#"data: {"p":"response/fragments/-1/content","o":"APPEND","v":" dunia"}"#,
            r#"data: {"v":"!"}"#,
            r#"data: {"p":"response/status","v":"FINISHED"}"#,
            r#"data: [DONE]"#,
        ];
        let mut text = String::new();
        let mut fin = false;
        for l in lines {
            for ev in p.feed_line(l) {
                match ev {
                    SseEvent::Text(t) => text.push_str(&t),
                    SseEvent::Finished => fin = true,
                    _ => {}
                }
            }
        }
        assert_eq!(text, "Halo dunia!");
        assert!(fin);
    }

    #[test]
    fn thinking_fragments_go_to_thinking() {
        // Fragment THINK/THINKING tidak boleh keluar sebagai teks jawaban.
        let mut p = SseParser::new();
        let evs = p.feed_line(
            r#"data: {"v":{"response":{"fragments":[{"type":"THINKING","content":"rahasia"}]}}}"#,
        );
        assert!(!evs.iter().any(|e| matches!(e, SseEvent::Text(_))));
        assert!(evs.contains(&SseEvent::Thinking("rahasia".into())));
    }

    #[test]
    fn bare_v_without_path_ignored() {
        let mut p = SseParser::new();
        assert!(p.feed_line(r#"data: {"v":"x"}"#).is_empty());
    }

    #[test]
    fn snapshot_with_id_and_content_yields_both() {
        // Bentuk nyata 11 Okt 2026: message_id + fragments RESPONSE sekaligus.
        let mut p = SseParser::new();
        let evs = p.feed_line(
            r#"data: {"v":{"response":{"message_id":2,"fragments":[{"id":2,"type":"RESPONSE","content":"2"}]}}}"#,
        );
        assert!(evs.contains(&SseEvent::MessageId(2)));
        assert!(evs.contains(&SseEvent::Text("2".into())));
        // Jangan duplikat saat frame APPEND berikutnya datang.
        let evs2 = p.feed_line(
            r#"data: {"p":"response/status","o":"SET","v":"FINISHED"}"#,
        );
        assert!(evs2.iter().any(|e| *e == SseEvent::Finished));
    }

    #[test]
    fn thinking_split_from_answer() {
        // Alur nyata thinking_enabled=true: snapshot THINK, appends reasoning,
        // lalu APPEND array berisi jawaban final RESPONSE.
        let mut p = SseParser::new();
        let lines = [
            r#"data: {"v":{"response":{"message_id":4,"fragments":[{"id":2,"type":"THINK","content":"We"}]}}}"#,
            r#"data: {"p":"response/fragments/-1/content","o":"APPEND","v":" need"}"#,
            r#"data: {"v":"144"}"#,
            r#"data: {"p":"response/fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"144"}]}"#,
            r#"data: {"p":"response/status","o":"SET","v":"FINISHED"}"#,
        ];
        let mut text = String::new();
        let mut think = String::new();
        for l in lines {
            for ev in p.feed_line(l) {
                match ev {
                    SseEvent::Text(t) => text.push_str(&t),
                    SseEvent::Thinking(t) => think.push_str(&t),
                    _ => {}
                }
            }
        }
        assert_eq!(think, "We need144");
        assert_eq!(text, "144");
    }
}
