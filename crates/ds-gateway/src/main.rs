//! deepshit: 1 binary Rust — REST + OpenAI-compat untuk chat.deepseek.com.
//! Pola meniru whatswrong: session per akun, arsip file JSONL, tanpa DB server.

mod manager;
mod openai;
mod ratelimit;

use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use clap::{Parser, Subcommand};
use ds_chat::DsChatClient;
use ds_core::{decode_cid, encode_cid, types::*, DsError};
use ds_store::archive::{Archive, JsonlArchive};
use manager::DsManager;
use ratelimit::{client_key, RateLimiter};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(name = "deepshit", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[arg(long, env = "DATA_DIR", default_value = "./ds-data")]
    data_dir: PathBuf,
    #[arg(long, env = "MEDIA_DIR", default_value = "./ds-media")]
    media_dir: PathBuf,
    #[arg(long, env = "LISTEN", default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    /// Default session checked at startup. "" = off (manual mode).
    #[arg(long, env = "SESSION_ID", default_value = "utama")]
    session_id: String,
    /// API token (Bearer). Empty = no auth (dev mode). Must be set in production.
    #[arg(long, env = "API_TOKEN", default_value = "")]
    api_token: String,
    /// Webhook URL for finished replies (POST JSON). Empty = off.
    #[arg(long, env = "WEBHOOK_URL", default_value = "")]
    webhook_url: String,
    /// Rate limit for /v1 per IP per minute.
    #[arg(long, env = "RATE_LIMIT_PER_MINUTE", default_value_t = 30)]
    rate_limit_per_minute: u32,
}

#[derive(Subcommand)]
enum Cmd {
    Setup {
        #[arg(long, default_value_t = false)]
        auto: bool,
    },
    Run,
    Migrate,
    /// Wipe all data: archive + media + sessions. Requires --yes.
    Reset {
        #[arg(long, default_value_t = false)]
        yes: bool,
        #[arg(long, default_value_t = false)]
        with_session: bool,
    },
}

#[derive(serde::Serialize)]
struct Health {
    ok: bool,
    version: &'static str,
    archive: &'static str,
    ds: &'static str,
}

struct AppState {
    manager: Arc<DsManager>,
    archive: Arc<dyn Archive>,
    archive_name: &'static str,
    media_dir: PathBuf,
    data_dir: PathBuf,
    api_token: String,
    webhook_url: String,
    limiter: RateLimiter,
    rate_limit: u32,
    /// response_message_id yang sedang berjalan per "akun/chat" (untuk stop).
    inflight: Arc<tokio::sync::Mutex<HashMap<String, i64>>>,
}

// --- auth guard ---

async fn auth_guard(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    if st.api_token.is_empty() || req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {}", st.api_token))
        .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// --- helpers ---

fn need_login() -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        "not logged in, POST /sessions/login {\"id\":\"<account>\",\"token\":\"<userToken>\"} first. Token comes from chat.deepseek.com (DevTools -> localStorage userToken / authorization header)".into(),
    )
}

fn map_ds_err(e: DsError) -> (StatusCode, String) {
    match e {
        DsError::NotAuthenticated(m) | DsError::LoginRequired(m) => (StatusCode::CONFLICT, m),
        DsError::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        DsError::RateLimited(_) => (StatusCode::BAD_GATEWAY, e.to_string()),
        DsError::Transport(m) => (StatusCode::BAD_GATEWAY, m),
        DsError::Protocol { code, text } => (StatusCode::BAD_GATEWAY, format!("upstream {code}: {text}")),
        DsError::Api(m) => (StatusCode::BAD_GATEWAY, m),
        DsError::Pow(m) => (StatusCode::BAD_GATEWAY, format!("PoW: {m}")),
        DsError::Store(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        DsError::Timeout(m) => (StatusCode::BAD_GATEWAY, m),
        DsError::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn map_any(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn chat_client(st: &Arc<AppState>, id: &str) -> Result<DsChatClient, (StatusCode, String)> {
    let sock = st.manager.get(id).await.map_err(|_| need_login())?;
    if !sock.is_logged_in().await {
        return Err(need_login());
    }
    Ok(DsChatClient::new(sock))
}

fn titles_path(data_dir: &PathBuf, account: &str) -> PathBuf {
    data_dir.join(account).join("titles.json")
}

async fn load_titles(data_dir: &PathBuf, account: &str) -> HashMap<String, String> {
    tokio::fs::read_to_string(titles_path(data_dir, account))
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

async fn save_title(data_dir: &PathBuf, account: &str, cid: &str, title: &str) {
    let mut m = load_titles(data_dir, account).await;
    m.insert(cid.to_string(), title.to_string());
    let p = titles_path(data_dir, account);
    let _ = tokio::fs::create_dir_all(p.parent().unwrap()).await;
    let _ = tokio::fs::write(&p, serde_json::to_vec_pretty(&m).unwrap_or_default()).await;
}

async fn remove_title(data_dir: &PathBuf, account: &str, cid: &str) {
    let mut m = load_titles(data_dir, account).await;
    m.remove(cid);
    let p = titles_path(data_dir, account);
    let _ = tokio::fs::write(&p, serde_json::to_vec_pretty(&m).unwrap_or_default()).await;
}

struct StoredMedia {
    sha256_hex: String,
    mimetype: &'static str,
    len: usize,
    path: PathBuf,
}

async fn store_media_bytes(
    media_dir: &PathBuf,
    account: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<StoredMedia, String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let hex = format!("{:x}", h.finalize());
    let safe: String = filename
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let dir = media_dir.join(account);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let path = dir.join(format!("{hex}-{safe}"));
    tokio::fs::write(&path, bytes).await.map_err(|e| e.to_string())?;
    let mime = if safe.to_lowercase().ends_with(".pdf") {
        "application/pdf"
    } else if safe.to_lowercase().ends_with(".png") {
        "image/png"
    } else if safe.to_lowercase().ends_with(".jpg") || safe.to_lowercase().ends_with(".jpeg") {
        "image/jpeg"
    } else if safe.to_lowercase().ends_with(".webp") {
        "image/webp"
    } else if safe.to_lowercase().ends_with(".gif") {
        "image/gif"
    } else if safe.to_lowercase().ends_with(".txt") || safe.to_lowercase().ends_with(".md") {
        "text/plain"
    } else {
        "application/octet-stream"
    };
    Ok(StoredMedia {
        sha256_hex: hex,
        mimetype: mime,
        len: bytes.len(),
        path,
    })
}

fn fire_webhook(url: &str, payload: serde_json::Value) {
    if url.is_empty() {
        return;
    }
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        match ureq::post(&url).send_json(&payload) {
            Ok(r) => tracing::info!(status = r.status(), "webhook ok"),
            Err(e) => tracing::warn!(err = %e, "webhook failed"),
        }
    });
}

fn chrono_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --- handlers: health/session ---

async fn health(State(st): State<Arc<AppState>>) -> Json<Health> {
    Json(Health {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        archive: st.archive_name,
        ds: "web (chat.deepseek.com)",
    })
}

#[derive(serde::Deserialize)]
struct LoginIn {
    id: String,
    token: String,
    cookies: Option<HashMap<String, String>>,
}

async fn login(
    State(st): State<Arc<AppState>>,
    Json(b): Json<LoginIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if b.id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "field id is required".into()));
    }
    if b.token.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "field token is required (browser userToken, see README)".into(),
        ));
    }
    st.manager
        .login_token(&b.id, &b.token, b.cookies.unwrap_or_default())
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({"id": b.id, "logged_in": true})))
}

async fn list_sessions(State(st): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(st.manager.list().await)
}

async fn session_status(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match st.manager.get(&id).await {
        Ok(s) => Ok(Json(serde_json::json!({
            "id": id,
            "logged_in": s.is_logged_in().await,
        }))),
        Err(_) => Err((StatusCode::NOT_FOUND, format!("session '{id}' does not exist, POST /sessions/login first"))),
    }
}

async fn session_refresh(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let s = st.manager.get(&id).await.map_err(|_| need_login())?;
    s.refresh().await.map_err(map_ds_err)?;
    Ok(Json(serde_json::json!({"id": id, "refreshed": true})))
}

async fn session_logout(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match st.manager.get(&id).await {
        Ok(s) => {
            s.logout().await.map_err(map_ds_err)?;
            st.manager.remove(&id).await;
            Ok(Json(serde_json::json!({"logged_out": id})))
        },
        Err(_) => Err((StatusCode::NOT_FOUND, format!("session '{id}' does not exist"))),
    }
}

// --- handlers: chat ---

async fn new_chat(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    let cid = c.create_session().await.map_err(map_ds_err)?;
    let _ = st.archive.save_message(&id, &format!("new-{cid}"), &cid, serde_json::json!({"type":"newchat"})).await;
    Ok(Json(serde_json::json!({"chat_id": cid})))
}

async fn list_chats(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    match c.list_sessions(1, 50).await {
        Ok(mut list) => {
            let titles = load_titles(&st.data_dir, &id).await;
            for ch in &mut list {
                if ch.title.is_none() {
                    ch.title = titles.get(&ch.id).cloned();
                }
            }
            Ok(Json(serde_json::json!({"chats": list})))
        },
        Err(e) => Err(map_ds_err(e)),
    }
}

async fn chat_history(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    // Upstream dulu; bila gagal (offline/mock), fallback ke arsip lokal.
    match c.history(&cid).await {
        Ok(msgs) => Ok(Json(serde_json::json!({"chat_id": cid, "messages": msgs}))),
        Err(_) => {
            let all = st.archive.recent(&id, 1000).await.map_err(map_ds_err)?;
            let msgs: Vec<_> = all.into_iter().filter(|m| m.jid == cid).collect();
            Ok(Json(serde_json::json!({"chat_id": cid, "messages": msgs, "cached": true})))
        },
    }
}

async fn delete_chat(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    // Upstream best-effort; arsip lokal selalu dibersihkan sebagian.
    let _ = c.delete_session(&cid).await;
    remove_title(&st.data_dir, &id, &cid).await;
    Ok(Json(serde_json::json!({"deleted": cid})))
}

#[derive(serde::Deserialize)]
struct RenameIn {
    title: String,
}

async fn rename_chat(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
    Json(b): Json<RenameIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _ = chat_client(&st, &id).await?;
    if b.title.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title is empty".into()));
    }
    // Upstream dulu (temuan UI web: POST /chat_session/update_title).
    // Bila upstream gagal, simpan lokal agar list gateway tetap rapi.
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    match c.rename(&cid, b.title.trim()).await {
        Ok(t) => {
            save_title(&st.data_dir, &id, &cid, &t).await;
            Ok(Json(serde_json::json!({"chat_id": cid, "title": t, "synced": true})))
        },
        Err(e) => {
            save_title(&st.data_dir, &id, &cid, b.title.trim()).await;
            let (code, msg) = map_ds_err(e);
            Ok(Json(serde_json::json!({"chat_id": cid, "title": b.title, "synced": false, "upstream_error": format!("{code}: {msg}")})))
        },
    }
}

#[derive(serde::Deserialize)]
struct SendIn {
    text: String,
    thinking: Option<bool>,
    search: Option<bool>,
    model: Option<String>,
    parent_message_id: Option<i64>,
    conversation_id: Option<String>,
}

async fn send_message(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
    Json(b): Json<SendIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if b.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is empty".into()));
    }
    // cid path vs conversation_id body: conversation_id menang bila ada (lanjut thread).
    let (chat_id, parent) = match b.conversation_id.as_deref() {
        Some(c) if !c.is_empty() => {
            let (s, p) = decode_cid(Some(c));
            (s.unwrap_or(cid.clone()), p.or(b.parent_message_id))
        },
        _ => (cid.clone(), b.parent_message_id),
    };
    let opt = SendOpt {
        thinking: b.thinking.unwrap_or(false),
        search: b.search.unwrap_or(false),
        model_type: b.model.as_deref().and_then(ds_core::types::resolve_model_type),
    };
    if let Some(m) = &b.model {
        if ds_core::types::resolve_model_type(m).is_none() {
            return Err((StatusCode::BAD_REQUEST, format!("unknown model: {m} (use deepseek-chat / deepseek-reasoner)")));
        }
    }
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    let reply: Reply = c.send(&chat_id, parent, &b.text, opt).await.map_err(map_ds_err)?;
    let msg_id = format!("out-{}", chrono_ms());
    let _ = st.archive.save_message(&id, &msg_id, &chat_id, serde_json::json!({"type":"out","text":b.text,"thinking":opt.thinking,"search":opt.search})).await;
    let _ = st.archive.save_message(&id, &format!("in-{msg_id}"), &chat_id, serde_json::json!({"type":"in","text":reply.text,"thinking":reply.thinking})).await;
    if !st.webhook_url.is_empty() {
        fire_webhook(&st.webhook_url, serde_json::json!({
            "event": "completion", "session": id, "chat_id": chat_id,
            "conversation_id": reply.conversation_id, "text": reply.text, "thinking": reply.thinking, "ts": chrono_ms(),
        }));
    }
    Ok(Json(serde_json::json!({"chat_id": chat_id, "text": reply.text, "thinking": reply.thinking, "conversation_id": reply.conversation_id})))
}

async fn send_message_stream(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
    Json(b): Json<SendIn>,
) -> Result<Response, (StatusCode, String)> {
    if b.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is empty".into()));
    }
    let (chat_id, parent) = match b.conversation_id.as_deref() {
        Some(c) if !c.is_empty() => {
            let (s, p) = decode_cid(Some(c));
            (s.unwrap_or(cid.clone()), p.or(b.parent_message_id))
        },
        _ => (cid.clone(), b.parent_message_id),
    };
    let opt = SendOpt {
        thinking: b.thinking.unwrap_or(false),
        search: b.search.unwrap_or(false),
        model_type: b.model.as_deref().and_then(ds_core::types::resolve_model_type),
    };
    let c = chat_client(&st, &id).await?;
    let (stream, msg_rx) = c.send_stream(chat_id.clone(), parent, b.text.clone(), opt).await.map_err(map_ds_err)?;
    // Simpan response_message_id untuk stop.
    {
        let inflight = st.inflight.clone();
        let key = format!("{id}/{chat_id}");
        tokio::spawn(async move {
            if let Ok(m) = msg_rx.await {
                inflight.lock().await.insert(key, m);
            }
        });
    }
    let st2 = st.clone();
    let id2 = id.clone();
    let s = async_stream::stream! {
        let mut pinned = stream;
        let mut full = String::new();
        while let Some(chunk) = futures::StreamExt::next(&mut pinned).await {
            match chunk {
                Ok(t) => {
                    full.push_str(&t);
                    let line = format!("data: {}\n\n", serde_json::to_string(&serde_json::json!({"delta": t})).unwrap());
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from(line));
                },
                Err(e) => {
                    let line = format!("data: {}\n\n", serde_json::to_string(&serde_json::json!({"error": e.to_string()})).unwrap());
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from(line));
                    break;
                },
            }
        }
        let _ = st2.archive.save_message(&id2, &format!("stream-{}", chrono_ms()), &chat_id, serde_json::json!({"type":"stream","text":full})).await;
        if !st2.webhook_url.is_empty() {
            fire_webhook(&st2.webhook_url, serde_json::json!({
                "event": "completion", "session": id2, "chat_id": chat_id, "text": full, "ts": chrono_ms(),
            }));
        }
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from("data: [DONE]\n\n"));
    };
    Ok(Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(s))
        .unwrap())
}

async fn stop_chat(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let c = chat_client(&st, &id).await?;
    let mid = st.inflight.lock().await.remove(&format!("{id}/{cid}"));
    c.stop_with_msg(&cid, mid).await.map_err(map_ds_err)?;
    Ok(Json(serde_json::json!({"stopped": cid, "message_id": mid})))
}

async fn upload_file(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    mut mp: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _ = chat_client(&st, &id).await?;
    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(f) = mp.next_field().await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))? {
        match f.name().unwrap_or("") {
            "filename" => filename = f.text().await.ok(),
            "file" => {
                filename = filename.or_else(|| f.file_name().map(|s| s.to_string()));
                bytes = f.bytes().await.ok().map(|b| b.to_vec());
            },
            _ => {},
        }
    }
    let (filename, bytes) = match (filename, bytes) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err((StatusCode::BAD_REQUEST, "multipart fields filename/file are required".into())),
    };
    // Tahap 1: arsip lokal + hash (memudahkan debug saat server tolak).
    let stored = store_media_bytes(&st.media_dir, &id, &filename, &bytes).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // Tahap 2: upload upstream.
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    match c.upload(bytes, &filename).await {
        Ok(fid) => {
            let _ = st.archive.save_message(&id, &format!("up-{fid}"), &fid, serde_json::json!({"type":"upload","filename":filename,"sha256":stored.sha256_hex})).await;
            Ok(Json(serde_json::json!({"file_id": fid, "filename": filename, "sha256": stored.sha256_hex, "mimetype": stored.mimetype, "len": stored.len})))
        },
        Err(e) => {
            // Bukti arsip lokal tetap ada walau upstream tolak (pelajaran wav→ogg).
            Err(map_ds_err(e))
        },
    }
}

/// Kirim + file dalam satu alur multipart (F4 bukti isi terbaca).
/// Fields: text, filename/file, thinking?, search?(harus false bila ada file).
async fn send_with_file(
    State(st): State<Arc<AppState>>,
    Path((id, cid)): Path<(String, String)>,
    mut mp: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut text: Option<String> = None;
    let mut thinking = false;
    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(f) = mp.next_field().await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))? {
        match f.name().unwrap_or("") {
            "text" => text = f.text().await.ok(),
            "thinking" => thinking = f.text().await.map(|v| v == "true" || v == "1").unwrap_or(false),
            "filename" => filename = f.text().await.ok(),
            "file" => {
                filename = filename.or_else(|| f.file_name().map(|s| s.to_string()));
                bytes = f.bytes().await.ok().map(|b| b.to_vec());
            },
            _ => {},
        }
    }
    let (text, filename, bytes) = match (text, filename, bytes) {
        (Some(t), Some(n), Some(b)) => (t, n, b),
        _ => return Err((StatusCode::BAD_REQUEST, "multipart fields text, filename/file are required".into())),
    };
    if text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is empty".into()));
    }
    // Tahap 1: arsip lokal + hash.
    let stored = store_media_bytes(&st.media_dir, &id, &filename, &bytes).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // Tahap 2: upload + completion satu alur (ref_file_ids valid).
    let _lk_holder = st.manager.lock_for(&id).await;
    let _lk = _lk_holder.lock().await;
    let c = chat_client(&st, &id).await?;
    let opt = SendOpt { thinking, search: false, model_type: None };
    match c.send_with_files(&cid, None, &text, opt, vec![(bytes, filename.clone())]).await {
        Ok(reply) => {
            let _ = st.archive.save_message(&id, &format!("fw-{}", chrono_ms()), &cid, serde_json::json!({"type":"with-file","filename":filename,"sha256":stored.sha256_hex,"text":reply.text})).await;
            if !st.webhook_url.is_empty() {
                fire_webhook(&st.webhook_url, serde_json::json!({
                    "event": "completion", "session": id, "chat_id": cid,
                    "conversation_id": reply.conversation_id, "text": reply.text, "ts": chrono_ms(),
                }));
            }
            Ok(Json(serde_json::json!({"chat_id": cid, "text": reply.text, "thinking": reply.thinking, "conversation_id": reply.conversation_id, "sha256": stored.sha256_hex})))
        },
        Err(e) => Err(map_ds_err(e)),
    }
}

async fn archive_recent(    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let msgs = st.archive.recent(&id, 50).await.map_err(map_ds_err)?;
    Ok(Json(serde_json::json!({"messages": msgs})))
}

// --- OpenAI-compat (F6) ---

#[derive(serde::Deserialize)]
struct OaiChatReq {
    model: Option<String>,
    messages: Vec<openai::ChatMessage>,
    stream: Option<bool>,
    conversation_id: Option<String>,
    thinking: Option<bool>,
    search: Option<bool>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<i64>,
}

fn oai_error(msg: String, typ: &str, status: StatusCode) -> Response {
    let body = serde_json::json!({"error": {"message": msg, "type": typ}});
    (status, Json(body)).into_response()
}

async fn oai_models(State(_st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let created = chrono_ms() / 1000;
    Json(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "deepseek-chat", "object": "model", "created": created, "owned_by": "deepseek"},
            {"id": "deepseek-reasoner", "object": "model", "created": created, "owned_by": "deepseek"},
        ],
    }))
}

async fn oai_chat(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<OaiChatReq>,
) -> Response {
    // Rate limit per IP untuk /v1 (429 + Retry-After standar).
    let peer = q.get("peer").cloned().unwrap_or_else(|| "unknown".into());
    let key = client_key(&headers, &peer);
    match st.limiter.hit(&key) {
        Ok(_) => {},
        Err(retry) => {
            let mut r = oai_error(
                format!("Rate limit exceeded: {} req/min. Retry in {retry:.1}s.", st.limiter_limit()),
                "rate_limit_error",
                StatusCode::TOO_MANY_REQUESTS,
            );
            r.headers_mut().insert("retry-after", axum::http::HeaderValue::from_str(&format!("{}", retry as u64 + 1)).unwrap());
            return r;
        },
    }
    if b.messages.is_empty() {
        return oai_error("`messages` must not be empty".into(), "invalid_request_error", StatusCode::BAD_REQUEST);
    }
    let model = b.model.unwrap_or_else(|| "deepseek-chat".into());
    if ds_core::types::resolve_model_type(&model).is_none() {
        return oai_error(
            format!("The model `{model}` does not exist. Available: deepseek-chat, deepseek-reasoner"),
            "model_not_found",
            StatusCode::NOT_FOUND,
        );
    }
    // Session OpenAI -> akun DeepSeek: header x-session-id atau SESSION_ID default,
    // atau query ?session=. Tanpa session -> 503 + cara login.
    let account = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| q.get("session").cloned())
        .unwrap_or_else(|| std::env::var("SESSION_ID").unwrap_or_else(|_| "utama".into()));
    // Default account from CLI; falls back to "utama" when env is empty.
    let account = if account.is_empty() { "utama".into() } else { account };
    let sock = match st.manager.get(&account).await {
        Ok(s) => s,
        Err(_) => {
            return oai_error(
                "No DeepSeek session. POST /sessions/login {\"id\":\"utama\",\"token\":\"<userToken>\"} first.".into(),
                "login_required",
                StatusCode::SERVICE_UNAVAILABLE,
            )
        },
    };
    if !sock.is_logged_in().await {
        return oai_error("DeepSeek session is not logged in.".into(), "login_required", StatusCode::CONFLICT);
    }
    let prompt = openai::messages_to_prompt(&b.messages);
    let thinking = b.thinking.unwrap_or(false);
    let search = b.search.unwrap_or(false);
    // conversation_id OpenAI (extra) -> thread DeepSeek. model diabaikan saat resume.
    let (chat_id_opt, parent) = decode_cid(b.conversation_id.as_deref());
    let model_type = if chat_id_opt.is_none() {
        ds_core::types::resolve_model_type(&model)
    } else {
        None
    };
    let opt = SendOpt { thinking, search, model_type };
    let client = DsChatClient::new(sock);
    if b.stream.unwrap_or(false) {
        // Buat / pastikan chat_id dulu (butuh lock singkat).
        let chat_id = match chat_id_opt {
            Some(s) => s,
            None => match client.create_session().await {
                Ok(s) => s,
                Err(e) => {
                    let (c, m) = map_ds_err(e);
                    return oai_error(m, "server_error", c);
                },
            },
        };
        let stream = match client.send_stream(chat_id.clone(), parent, prompt.clone(), opt).await {
            Ok((s, msg_rx)) => {
                let inflight = st.inflight.clone();
                let key = format!("{account}/{chat_id}");
                tokio::spawn(async move {
                    if let Ok(m) = msg_rx.await {
                        inflight.lock().await.insert(key, m);
                    }
                });
                s
            },
            Err(e) => {
                let (c, m) = map_ds_err(e);
                return oai_error(m, "server_error", c);
            },
        };
        let created = chrono_ms() / 1000;
        let cid0 = format!("chatcmpl-{:x}", chrono_ms());
        let model2 = model.clone();
        let s = async_stream::stream! {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(openai::sse_frame(&model2, &cid0, created as i64, serde_json::json!({"role":"assistant","content":""}), None, None)));
            let mut pinned = stream;
            let mut full = String::new();
            while let Some(chunk) = futures::StreamExt::next(&mut pinned).await {
                match chunk {
                    Ok(t) => {
                        if t.is_empty() { continue; }
                        full.push_str(&t);
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(openai::sse_frame(&model2, &cid0, created as i64, serde_json::json!({"content": t}), None, None)));
                    },
                    Err(_e) => {
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(openai::sse_frame(&model2, &cid0, created as i64, serde_json::json!({}), Some("stop"), None)));
                        break;
                    },
                }
            }
            // conversation_id tidak diketahui tanpa message_id upstream; pakai chat_id.
            let conv = encode_cid(&chat_id, None);
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(openai::sse_frame(&model2, &cid0, created as i64, serde_json::json!({}), Some("stop"), Some(&conv))));
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from("data: [DONE]\n\n"));
        };
        return Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(s))
            .unwrap();
    }
    // Non-stream: chat baru atau lanjutan.
    let (chat_id, parent) = match (chat_id_opt, parent) {
        (Some(s), p) => (s, p),
        (None, _) => match client.create_session().await {
            Ok(s) => (s, None),
            Err(e) => {
                let (c, m) = map_ds_err(e);
                return oai_error(m, "server_error", c);
            },
        },
    };
    match client.send(&chat_id, parent, &prompt, opt).await {
        Ok(reply) => {
            let v = openai::completion_response_with_thinking(&model, &reply.text, reply.thinking.as_deref(), &prompt, &reply.conversation_id);
            Json(v).into_response()
        },
        Err(e) => {
            let (c, m) = map_ds_err(e);
            oai_error(m, "server_error", c)
        },
    }
}

// --- setup/run/reset ---

fn ensure_dir(dir: &PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

fn archive_dir(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("archive")
}

async fn setup_auto(media_dir: &PathBuf, data_dir: &PathBuf) -> anyhow::Result<()> {
    ensure_dir(media_dir)?;
    ensure_dir(data_dir)?;
    ensure_dir(&archive_dir(data_dir))?;
    println!(
        "setup ok: file mode (JSONL archive in {}/ + media in {} + sessions in {}).",
        archive_dir(data_dir).display(),
        media_dir.display(),
        data_dir.display()
    );
    Ok(())
}

async fn reset_all(media_dir: &PathBuf, data_dir: &PathBuf, yes: bool, with_session: bool) -> anyhow::Result<()> {
    if !yes {
        println!("Aborted: add --yes to reset. Example: deepshit reset --yes");
        return Ok(());
    }
    for d in [media_dir, &archive_dir(data_dir)] {
        if d.exists() {
            std::fs::remove_dir_all(d)?;
            println!("Removed: {}", d.display());
        }
    }
    ensure_dir(media_dir)?;
    ensure_dir(&archive_dir(data_dir))?;
    if with_session {
        for e in std::fs::read_dir(data_dir)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
                println!("Session removed: {}", p.display());
            }
        }
    }
    println!("Reset done — fresh state.");
    Ok(())
}

impl AppState {
    fn limiter_limit(&self) -> u32 {
        self.rate_limit
    }
}

async fn run_server(
    media_dir: &PathBuf,
    data_dir: &PathBuf,
    listen: &SocketAddr,
    auto_session: &str,
    api_token: &str,
    webhook_url: &str,
    rate_limit: u32,
) -> anyhow::Result<()> {
    ensure_dir(media_dir)?;
    ensure_dir(data_dir)?;
    let adir = archive_dir(data_dir);
    ensure_dir(&adir)?;
    let archive: Arc<dyn Archive> = Arc::new(JsonlArchive::new(adir));
    let manager = Arc::new(DsManager::new(data_dir.clone()));
    if !auto_session.is_empty() {
        match manager.get(auto_session).await {
            Ok(s) if s.is_logged_in().await => {
                println!("default session '{auto_session}' is logged in, ready.");
            },
            _ => {
                println!("default session '{auto_session}' is not logged in — POST /sessions/login {{\"id\":\"{auto_session}\",\"token\":\"...\"}} untuk mengaktifkan.");
            },
        }
        // Watchdog refresh-token: cek tiap 5 menit, log bila mati.
        let m = manager.clone();
        let sid = auto_session.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                match m.get(&sid).await {
                    Ok(s) => {
                        if let Err(e) = s.refresh().await {
                            tracing::warn!(err = %e, session = %sid, "watchdog: refresh failed");
                        }
                    },
                    Err(_) => {},
                }
            }
        });
    }
    // Buat AppState (rate_limit disimpan untuk pesan error 429).
    let st = Arc::new(AppState {
        manager,
        archive,
        archive_name: "file",
        media_dir: media_dir.clone(),
        data_dir: data_dir.clone(),
        api_token: api_token.to_string(),
        webhook_url: webhook_url.to_string(),
        limiter: RateLimiter::new(rate_limit, 60),
        rate_limit,
        inflight: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    });
    let app: Router = Router::new()
        .route("/health", get(health))
        .route("/sessions/login", post(login))
        .route("/sessions", get(list_sessions))
        .route("/sessions/:id/status", get(session_status))
        .route("/sessions/:id/refresh", post(session_refresh))
        .route("/sessions/:id", delete(session_logout))
        .route("/sessions/:id/chats", post(new_chat).get(list_chats))
        .route("/sessions/:id/chats/:cid/messages", get(chat_history).post(send_message))
        .route("/sessions/:id/chats/:cid/messages/stream", post(send_message_stream))
        .route("/sessions/:id/chats/:cid/messages/with-files", post(send_with_file))
        .route("/sessions/:id/chats/:cid/stop", post(stop_chat))
        .route("/sessions/:id/chats/:cid", delete(delete_chat))
        .route("/sessions/:id/chats/:cid/rename", post(rename_chat))
        .route("/sessions/:id/upload", post(upload_file))
        .route("/sessions/:id/messages", get(archive_recent))
        .route("/v1/models", get(oai_models))
        .route("/v1/chat/completions", post(oai_chat))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(axum::middleware::from_fn_with_state(st.clone(), auth_guard))
        .with_state(st);
    println!(
        "listen {listen} (arsip: file, auth: {})",
        if api_token.is_empty() { "mati (dev)" } else { "Bearer ON" }
    );
    println!("DeepSeek web ready: login via POST /sessions/login, then send.");
    println!("OpenAI-compat: POST /v1/chat/completions + GET /v1/models (base_url http://{listen}/v1).");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// --- main ---

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            let _ = setup_auto(&cli.media_dir, &cli.data_dir).await;
            run_server(
                &cli.media_dir,
                &cli.data_dir,
                &cli.listen,
                &cli.session_id,
                &cli.api_token,
                &cli.webhook_url,
                cli.rate_limit_per_minute,
            )
            .await?;
        },
        Some(Cmd::Setup { auto: _ }) => {
            setup_auto(&cli.media_dir, &cli.data_dir).await?;
        },
        Some(Cmd::Migrate) => {
            println!("file mode: nothing to migrate.");
        },
        Some(Cmd::Reset { yes, with_session }) => {
            reset_all(&cli.media_dir, &cli.data_dir, yes, with_session).await?;
        },
        Some(Cmd::Run) => {
            run_server(
                &cli.media_dir,
                &cli.data_dir,
                &cli.listen,
                &cli.session_id,
                &cli.api_token,
                &cli.webhook_url,
                cli.rate_limit_per_minute,
            )
            .await?;
        },
    }
    Ok(())
}
