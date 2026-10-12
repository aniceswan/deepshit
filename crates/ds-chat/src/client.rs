use ds_auth::DsSocket;
use ds_core::{
    constants::*, pow::solve_and_build_header, sse::SseParser, types::*, DsError,
};
use futures::Stream;
use std::{pin::Pin, sync::Arc};
use tokio::sync::Mutex;

pub struct DsChatClient {
    socket: Arc<DsSocket>,
    /// Serialisasi request upstream per akun (PoW solver + hindari hammer).
    /// Meniru antrean per session whatswrong + lock PoW sums001.
    lock: Mutex<()>,
}

impl DsChatClient {
    pub fn new(socket: Arc<DsSocket>) -> Self {
        Self {
            socket,
            lock: Mutex::new(()),
        }
    }

    pub fn socket(&self) -> &Arc<DsSocket> {
        &self.socket
    }

    fn url(path: &str) -> String {
        format!("{BASE_URL}{path}")
    }

    /// Unwrap envelope `{"code":0,"data":{"biz_code":0,"biz_data":{...}}}`.
    /// biz_code != 0 (mis. 5 EMPTY_CHAT_SESSION) = error dengan biz_msg.
    fn biz(v: serde_json::Value) -> Result<serde_json::Value, DsError> {
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown api error");
            return Err(DsError::Api(format!("code {code}: {msg}")));
        }
        let data = v.get("data");
        let biz_code = data
            .and_then(|d| d.get("biz_code"))
            .and_then(|c| c.as_i64())
            .unwrap_or(0);
        if biz_code != 0 {
            let msg = data
                .and_then(|d| d.get("biz_msg"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown biz error");
            return Err(DsError::Api(format!("biz_code {biz_code}: {msg}")));
        }
        v.pointer("/data/biz_data")
            .cloned()
            .ok_or_else(|| DsError::Api(format!("unrecognized response shape: {v}")))
    }

    // --- manajemen chat ---

    /// Buat chat baru (newchat). Mengembalikan chat_session_id.
    pub async fn create_session(&self) -> Result<String, DsError> {
        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .post(Self::url(PATH_CREATE_SESSION))
            .headers(headers)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let st = r.status().as_u16();
        if st == 401 || st == 403 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        biz.pointer("/chat_session/id")
            .or_else(|| biz.pointer("/id"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DsError::Api("create_session returned no id".into()))
    }

    /// List chat (fetch_page via GET). Mengembalikan daftar chat_session.
    pub async fn list_sessions(&self, _page: u32, _page_size: u32) -> Result<Vec<ChatInfo>, DsError> {
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .get(Self::url(PATH_FETCH_SESSIONS))
            .headers(headers)
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        if r.status().as_u16() == 401 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        // Bentuk nyata: {chat_sessions: [{id, title, ...}]}
        let arr = biz
            .get("chat_sessions")
            .or_else(|| biz.get("sessions"))
            .or_else(|| biz.get("list"))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(arr
            .iter()
            .filter_map(|s| {
                Some(ChatInfo {
                    id: s.get("id")?.as_str()?.to_string(),
                    title: s.get("title").and_then(|t| t.as_str()).map(|x| x.to_string()),
                    created_at: s.get("updated_at").and_then(|x| x.as_f64()).map(|x| x as u64),
                })
            })
            .collect())
    }

    /// Riwayat pesan satu chat (GET history_messages).
    /// Bentuk nyata: {chat_session: {...}, chat_messages: [{message_id, role, fragments:[{type,content}]}]}
    pub async fn history(&self, chat_session_id: &str) -> Result<Vec<ChatMsg>, DsError> {
        let headers = self.socket.base_headers().await?;
        let url = format!("{}{PATH_HISTORY}?chat_session_id={chat_session_id}", BASE_URL);
        let r = self.socket.http.get(&url).headers(headers).send().await.map_err(|e| DsError::Transport(e.to_string()))?;
        if r.status().as_u16() == 401 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        // Bentuk nyata chat_messages; fallback ke tebakan lama bila berubah.
        if let Some(arr) = biz.get("chat_messages").and_then(|x| x.as_array()) {
            let mut out = vec![];
            for m in arr {
                let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("unknown").to_string();
                let mid = m.get("message_id").and_then(|x| x.as_i64());
                let mut content = String::new();
                if let Some(frags) = m.get("fragments").and_then(|x| x.as_array()) {
                    for f in frags {
                        let t = f.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        // Gabung REQUEST + RESPONSE + THINKING (diberi label).
                        if t == "THINKING" {
                            if let Some(c) = f.get("content").and_then(|x| x.as_str()) {
                                if !content.is_empty() {
                                    content.push_str("\n\n");
                                }
                                content.push_str("[thinking] ");
                                content.push_str(c);
                            }
                        } else if let Some(c) = f.get("content").and_then(|x| x.as_str()) {
                            content.push_str(c);
                        }
                    }
                }
                if content.is_empty() {
                    content = m
                        .get("content")
                        .and_then(|x| x.as_str())
                        .or_else(|| m.get("text").and_then(|x| x.as_str()))
                        .unwrap_or("")
                        .to_string();
                }
                out.push(ChatMsg { role, content, message_id: mid });
            }
            return Ok(out);
        }
        let arr = biz
            .get("messages")
            .or_else(|| biz.get("history"))
            .or_else(|| biz.get("list"))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(arr
            .iter()
            .map(|m| ChatMsg {
                role: m
                    .get("role")
                    .and_then(|x| x.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                content: m
                    .get("content")
                    .and_then(|x| x.as_str())
                    .or_else(|| m.get("text").and_then(|x| x.as_str()))
                    .unwrap_or("")
                    .to_string(),
                message_id: m.get("message_id").and_then(|x| x.as_i64()),
            })
            .collect())
    }

    async fn pow_header_inner(
        &self,
        _headers: &reqwest::header::HeaderMap,
        target_path: &str,
    ) -> Result<String, DsError> {
        // Dipanggil saat lock sudah dipegang (hindari deadlock): buat request PoW
        // tanpa mengunci lagi.
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .post(Self::url(PATH_POW_CHALLENGE))
            .headers(headers)
            .json(&serde_json::json!({"target_path": target_path}))
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        let ch: Challenge = serde_json::from_value(
            biz.pointer("/challenge").cloned().unwrap_or_else(|| biz.clone()),
        )
        .map_err(|e| DsError::Api(format!("bad challenge: {e}")))?;
        let ch = if ch.algorithm.is_empty() {
            serde_json::from_value::<Challenge>(biz)
                .map_err(|e| DsError::Api(format!("bad challenge: {e}")))?
        } else {
            ch
        };
        tokio::task::spawn_blocking(move || solve_and_build_header(&ch))
            .await
            .map_err(|e| DsError::Pow(e.to_string()))?
    }

    /// Ganti judul chat (temuan reverse-engineer UI web, terverifikasi live):
    /// POST /api/v0/chat_session/update_title {chat_session_id, title}.
    pub async fn rename(&self, chat_session_id: &str, title: &str) -> Result<String, DsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DsError::Invalid("title is empty".into()));
        }
        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .post(Self::url(PATH_UPDATE_TITLE))
            .headers(headers)
            .json(&serde_json::json!({"chat_session_id": chat_session_id, "title": title}))
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        if r.status().as_u16() == 401 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        biz.pointer("/title")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DsError::Api(format!("update_title tanpa title: {biz}")))
    }

    /// Hapus chat.
    pub async fn delete_session(&self, chat_session_id: &str) -> Result<(), DsError> {        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .post(Self::url(PATH_DELETE_SESSION))
            .headers(headers)
            .json(&serde_json::json!({"chat_session_id": chat_session_id}))
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        if r.status().as_u16() == 401 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        Self::biz(v)?;
        Ok(())
    }

    /// Hentikan generate yang berjalan. Bila response_message_id diketahui
    /// (dari frame pertama stream), sertakan agar server tahu persis yang dihentikan.
    pub async fn stop(&self, chat_session_id: &str) -> Result<(), DsError> {
        self.stop_with_msg(chat_session_id, None).await
    }

    pub async fn stop_with_msg(
        &self,
        chat_session_id: &str,
        message_id: Option<i64>,
    ) -> Result<(), DsError> {
        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let r = self
            .socket
            .http
            .post(Self::url(PATH_STOP_STREAM))
            .headers(headers)
            .json(&serde_json::json!({"chat_session_id": chat_session_id, "message_id": message_id}))
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let _ = r.text().await;
        Ok(())
    }

    // --- kirim ---

    fn completion_body(
        chat_session_id: &str,
        parent_id: Option<i64>,
        prompt: &str,
        opt: SendOpt,
        ref_file_ids: Vec<String>,
    ) -> serde_json::Value {
        let mut b = serde_json::json!({
            "chat_session_id": chat_session_id,
            "parent_message_id": parent_id,
            "prompt": prompt,
            "ref_file_ids": ref_file_ids,
            "thinking_enabled": opt.thinking,
            "search_enabled": opt.search,
            "action": null,
            "preempt": false,
        });
        if let Some(m) = opt.model_type {
            b["model_type"] = serde_json::Value::String(m.as_wire().into());
        }
        b
    }

    /// Kirim non-stream: kumpulkan SSE sampai FINISED (F2).
    /// Thinking (bila thinking_enabled) dipisah ke `Reply.thinking`.
    pub async fn send(
        &self,
        chat_session_id: &str,
        parent_id: Option<i64>,
        prompt: &str,
        opt: SendOpt,
    ) -> Result<Reply, DsError> {
        let (text, thinking, mid) = self.send_collect(chat_session_id, parent_id, prompt, opt, vec![]).await?;
        Ok(Reply {
            text,
            thinking,
            conversation_id: encode_cid(chat_session_id, mid),
        })
    }

    async fn send_collect(
        &self,
        chat_session_id: &str,
        parent_id: Option<i64>,
        prompt: &str,
        opt: SendOpt,
        ref_file_ids: Vec<String>,
    ) -> Result<(String, Option<String>, Option<i64>), DsError> {
        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let pow = self.pow_header_inner(&headers, PATH_COMPLETION).await?;
        let body = Self::completion_body(chat_session_id, parent_id, prompt, opt, ref_file_ids);
        let r = self
            .socket
            .http
            .post(Self::url(PATH_COMPLETION))
            .headers(headers)
            .header(HDR_POW, pow)
            .json(&body)
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let st = r.status().as_u16();
        if st == 401 || st == 403 {
            return Err(DsError::NotAuthenticated("token is dead at completion time (401)".into()));
        }
        if st == 429 {
            return Err(DsError::RateLimited(60.0));
        }
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(DsError::Protocol {
                code: st,
                text: t.chars().take(300).collect(),
            });
        }
        let text = r.text().await.map_err(|e| DsError::Transport(e.to_string()))?;
        if std::env::var("DSW_DUMP_SSE").is_ok() {
            let _ = std::fs::write("/tmp/opencode/dsw-raw-sse.txt", &text);
        }
        // Completion yang gagal di level aplikasi mengembalikan JSON envelope
        // (HTTP 200) alih-alih SSE — mis. biz_code 9 "invalid ref file id".
        // Deteksi agar tidak disalahartikan sebagai balasan kosong.
        if !text.lines().any(|l| l.trim_start().starts_with("data:")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                // biz_code/biz_msg sejajar dengan biz_data di bawah /data.
                let data = v.get("data");
                let biz_code = data
                    .and_then(|d| d.get("biz_code"))
                    .and_then(|c| c.as_i64())
                    .unwrap_or(0);
                let biz_msg = data
                    .and_then(|d| d.get("biz_msg"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                if code != 0 || biz_code != 0 {
                    let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or(biz_msg);
                    return Err(DsError::Api(format!("completion rejected (code {code}/biz {biz_code}: {msg})")));
                }
                // JSON 200 tanpa data: baris — tetap error jelas, bukan kosong.
                return Err(DsError::Api(format!(
                    "completion returned no stream: {}",
                    text.chars().take(200).collect::<String>()
                )));
            }
        }
        let mut parser = SseParser::new();
        let mut out = String::new();
        let mut think = String::new();
        let mut mid = None;
        for line in text.lines() {
            for ev in parser.feed_line(line) {
                match ev {
                    ds_core::sse::SseEvent::Text(t) => out.push_str(&t),
                    ds_core::sse::SseEvent::Thinking(t) => think.push_str(&t),
                    ds_core::sse::SseEvent::MessageId(m) => mid = Some(m),
                    _ => {},
                }
            }
        }
        Ok((out, if think.is_empty() { None } else { Some(think) }, mid))
    }

    /// Kirim streaming: mengembalikan (stream delta teks, receiver message_id).
    /// Receiver memberi response_message_id dari frame pertama (dipakai stop).
    /// Pemakaian: `let (s, rx) = client.send_stream(...).await?; ...`
    pub async fn send_stream(
        &self,
        chat_session_id: String,
        parent_id: Option<i64>,
        prompt: String,
        opt: SendOpt,
    ) -> Result<
        (
            Pin<Box<dyn Stream<Item = Result<String, DsError>> + Send>>,
            tokio::sync::oneshot::Receiver<i64>,
        ),
        DsError,
    > {
        // Kunci dipertahankan selama stream? Tidak — PoW + request dibuat di sini,
        // body di-stream tanpa lock (lock upstream sudah serial peritra? Kami
        // sengaja TIDAK menahan lock selama stream agar stop() bisa masuk.
        // Serialisasi kasar tetap ada via PoW yang cepat; dokumentasikan bahwa
        // konkurensi upstream dibatasi 1 per akun di gateway (lihat DsManager).
        let headers = self.socket.base_headers().await?;
        // PoW butuh lock? pow_header_inner tidak mengunci (aman).
        let pow = self.pow_header_inner(&headers, PATH_COMPLETION).await?;
        let body = Self::completion_body(&chat_session_id, parent_id, &prompt, opt, vec![]);
        let resp = self
            .socket
            .http
            .post(Self::url(PATH_COMPLETION))
            .headers(headers)
            .header(HDR_POW, pow)
            .json(&body)
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let st = resp.status().as_u16();
        if st == 401 || st == 403 {
            return Err(DsError::NotAuthenticated("token is dead at completion time (401)".into()));
        }
        if st == 429 {
            return Err(DsError::RateLimited(60.0));
        }
        if !resp.status().is_success() {
            let t = resp.text().await.unwrap_or_default();
            return Err(DsError::Protocol {
                code: st,
                text: t.chars().take(300).collect(),
            });
        }
        let stream = resp.bytes_stream();
        let (msg_tx, msg_rx) = tokio::sync::oneshot::channel::<i64>();
        let s = async_stream::stream! {
            let mut parser = SseParser::new();
            let mut buf = String::new();
            let mut pinned = Box::pin(stream);
            let mut msg_tx = Some(msg_tx);
            while let Some(chunk) = futures::StreamExt::next(&mut pinned).await {
                match chunk {
                    Err(e) => { yield Err(DsError::Transport(e.to_string())); break; },
                    Ok(b) => {
                        buf.push_str(&String::from_utf8_lossy(&b));
                        // Proses per baris; simpan sisa tak lengkap.
                        let mut lines: Vec<String> = buf.lines().map(|l| l.to_string()).collect();
                        let ends_nl = buf.ends_with('\n');
                        if !ends_nl { buf = lines.pop().unwrap_or_default(); } else { buf = String::new(); }
                        for l in lines {
                            for ev in parser.feed_line(&l) {
                                match ev {
                                    ds_core::sse::SseEvent::Text(t) if !t.is_empty() => yield Ok(t),
                                    ds_core::sse::SseEvent::Finished => {},
                                    ds_core::sse::SseEvent::MessageId(m) => {
                                        if let Some(tx) = msg_tx.take() {
                                            let _ = tx.send(m);
                                        }
                                    },
                                    _ => {},
                                }
                            }
                        }
                    }
                }
            }
            if !buf.is_empty() {
                for ev in parser.feed_line(&buf) {
                    if let ds_core::sse::SseEvent::Text(t) = ev { if !t.is_empty() { yield Ok(t); } }
                }
            }
        };
        Ok((Box::pin(s), msg_rx))
    }

    // --- upload (F4) ---

    /// Upload dua tahap: arsip lokal + hash dulu (di gateway), baru POST upstream.
    /// Mengembalikan file_id untuk dipakai sebagai ref_file_ids.
    pub async fn upload(&self, bytes: Vec<u8>, filename: &str) -> Result<String, DsError> {
        if bytes.is_empty() {
            return Err(DsError::Invalid("file is empty".into()));
        }
        if bytes.len() > 20 * 1024 * 1024 {
            return Err(DsError::Invalid("files over 20MB are rejected".into()));
        }
        let _g = self.lock.lock().await;
        let headers = self.socket.base_headers().await?;
        let pow = self.pow_header_inner(&headers, PATH_UPLOAD).await?;
        let mime = guess_mime(filename);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| DsError::Invalid(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        // Header multipart: jangan paksa content-type json.
        let mut h = headers;
        h.remove(reqwest::header::CONTENT_TYPE);
        let r = self
            .socket
            .http
            .post(Self::url(PATH_UPLOAD))
            .headers(h)
            .header(HDR_POW, pow)
            .multipart(form)
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        let st = r.status().as_u16();
        if st == 401 || st == 403 {
            return Err(DsError::NotAuthenticated("token is dead at upload time (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        if std::env::var("DSW_DUMP_SSE").is_ok() {
            let _ = std::fs::write("/tmp/opencode/dsw-raw-upload.txt", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        let biz = Self::biz(v)?;
        biz.pointer("/file_id")
            .or_else(|| biz.pointer("/file/id"))
            .or_else(|| biz.pointer("/id"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DsError::Api(format!("upload returned no file_id: {biz}")))
    }

    /// Status file: GET /file/fetch_files?file_ids=<id> -> "SUCCESS"/"PARSING"/"PENDING"/...
    pub async fn file_status(&self, file_id: &str) -> Result<Option<String>, DsError> {
        let headers = self.socket.base_headers().await?;
        let url = format!("{}{PATH_FETCH_FILES}?file_ids={file_id}", BASE_URL);
        let r = self
            .socket
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| DsError::Transport(e.to_string()))?;
        if r.status().as_u16() == 401 {
            return Err(DsError::NotAuthenticated("token is dead (401)".into()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| DsError::Transport(e.to_string()))?;
        let biz = Self::biz(v)?;
        let files = biz.get("files").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        Ok(files
            .iter()
            .find(|f| f.get("id").and_then(|x| x.as_str()) == Some(file_id))
            .and_then(|f| f.get("status"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()))
    }

    /// Tunggu file READY (poll sampai SUCCESS). PENDING/PARSING = tunggu;
    /// status lain / hilang = gagal.
    pub async fn wait_file_ready(&self, file_id: &str, timeout_secs: u64) -> Result<(), DsError> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        loop {
            match self.file_status(file_id).await? {
                Some(s) if s == "SUCCESS" => return Ok(()),
                Some(s) if s == "PENDING" || s == "PARSING" || s == "PROCESSING" => {},
                Some(s) => {
                    return Err(DsError::Api(format!("file {file_id} status: {s}")));
                },
                None => {
                    return Err(DsError::Api(format!("file {file_id} disappeared while polling")));
                },
            }
            if start.elapsed() > timeout {
                return Err(DsError::Timeout(format!("file {file_id} not SUCCESS after {timeout_secs}s")));
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
    /// Kirim dengan lampiran file (upload dulu, lalu completion dengan ref_file_ids).
    pub async fn send_with_files(
        &self,
        chat_session_id: &str,
        parent_id: Option<i64>,
        prompt: &str,
        opt: SendOpt,
        files: Vec<(Vec<u8>, String)>,
    ) -> Result<Reply, DsError> {
        if opt.search && !files.is_empty() {
            return Err(DsError::Invalid("upload + search can't be combined (server rule)".into()));
        }
        let mut ids = vec![];
        for (b, name) in files {
            ids.push(self.upload(b, &name).await?);
        }
        // File baru berstatus PENDING/PARSING — tunggu SUCCESS dulu
        // (terbukti live: txt 142 byte butuh ~3 menit). Baru completion.
        for f in &ids {
            self.wait_file_ready(f, 300).await?;
        }
        let (text, thinking, mid) = self.send_collect(chat_session_id, parent_id, prompt, opt, ids).await?;
        Ok(Reply {
            text,
            thinking,
            conversation_id: encode_cid(chat_session_id, mid),
        })
    }
}

fn guess_mime(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        "text/plain"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_flags() {
        let b = DsChatClient::completion_body("s1", Some(3), "halo", SendOpt { thinking: true, search: false, model_type: Some(ModelType::Expert) }, vec![]);
        assert_eq!(b["thinking_enabled"], true);
        assert_eq!(b["model_type"], "expert");
        assert_eq!(b["parent_message_id"], 3);
        let b2 = DsChatClient::completion_body("s1", None, "halo", SendOpt::default(), vec![]);
        assert!(b2.get("model_type").is_none());
    }
    #[test]
    fn mime_guess() {
        assert_eq!(guess_mime("a.pdf"), "application/pdf");
        assert_eq!(guess_mime("foto.JPG"), "image/jpeg");
        assert_eq!(guess_mime("x.xyz"), "application/octet-stream");
    }
}
