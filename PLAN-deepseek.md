# IMPLEMENTATION GUIDE — DeepSeek Web Chat API (Rust)

> End result: new Rust repo, 1 binary `deepshit`, same shape as `whatswrong`.
> Status: **done** — F0–F6 implemented and verified live. This file is the
> original step-by-step plan, kept for provenance.

## What "OpenAI-compatible" means

Instead of a custom API, the server mimics OpenAI's shape:

```bash
POST /v1/chat/completions
{ "model": "deepseek-chat",
  "messages": [{"role":"user","content":"hello"}],
  "stream": true }
```

Replies stream as SSE (one `data: {...}` per chunk, ending with
`data: [DONE]`), plus `GET /v1/models`. Win: **any tool that talks to
OpenAI (opencode, the openai SDK, LibreChat, …) works with this server**
— just change `base_url` to `http://localhost:3000/v1`, any API key.
Cost: web concepts (saved chats, thinking, search) must be squeezed into
the OpenAI format (e.g. `conversation_id` as an extra field, thinking as
plain text / `reasoning_content`), and OpenAI features the web lacks
(function calling) can't be supported. Advice: build plain REST first
(F2–F4), add OpenAI compatibility later as a thin layer (F6).

> Status: planning DRAFT, now completed. Backend: `chat.deepseek.com`
> (web chat, not the paid `api.deepseek.com`).
> Pattern: mirrors `whatswrong` — 1 Rust binary, session per account,
> REST + file archive, no DB server.

## 0. Core idea

DeepSeek web needs login. We wrap 1 logged-in account into 1 API session,
like `utama` in whatswrong:

```text
client --REST--> ds-gateway --HTTP+cookie--> chat.deepseek.com
```

- Each API request (`POST /sessions/:id/chats/:chat_id/messages`) is
  forwarded as a web-chat message; the (streaming) response goes back
  to the caller.
- Login state (cookie/token) is stored per session on disk
  (`./ds-data/<id>/`), like per-number sqlite in whatswrong —
  login once, reuse forever.
- Web-style chat management: `newchat`, list chats, reopen old chats
  (continue context), delete chats, rename titles.

## 1. Research first (Phase 0, mandatory before coding) ✅ done

DeepSeek publishes no docs for its web chat. Mapped via browser DevTools
(Network tab) + reference projects (`sums001/Deepseek-API`):

1. **Login flow**: which method (email+password? Google OAuth? SMS?).
   Login endpoint, CAPTCHA/Turnstile or not, 2FA.
2. **Session artifacts**: which cookie / `Authorization: Bearer` /
   localStorage rides on every chat request. Token age + refresh.
3. **Chat endpoints**: new chat, send, streaming response (SSE?),
   stop generation, delete chat, rename, history list, models used
   (`deepseek-chat` vs `deepseek-reasoner`, thinking/search flags).
4. **Limits**: rate limits, web context cap, dead-token behavior
   (401? login redirect?), bot detection / IP blocks.

Phase-0 output: `deepseek-protocol.md` with endpoint list + raw
request/response samples (sanitized, no credentials).

**Honest finding**: email+password over plain HTTP is dead (WAF blocks
even real browsers with `202` → `403`). Login = browser token import.

## 2. Locked decisions

* Login: **browser token import** (email+password removed — WAF-blocked).
* Initial scope: **text chat + file/image upload**. OpenAI-compat after (F6).
* Repo: **new, done**. Mirrors whatswrong's shape, not its code
  (totally different protocol: HTTP+cookie, not Noise/Signal).

## 3. Repo shape (whatswrong twin)

```text
deepshit/
  Cargo.toml            # workspace + release profile (lto, strip, opt-level 3)
  crates/
    ds-core             # types: Session, Chat, Msg; DsError; endpoint constants
    ds-auth             # token login, file-stored token, refresh on 401
    ds-chat             # client: newchat/list/get/delete/rename/send/stream/stop/upload
    ds-store            # Archive trait + JsonlArchive (wa-store idea)
    ds-gateway          # `deepshit` binary: axum REST + setup/reset + webhook + Bearer
  scripts/local-up.sh   # 1-command: build + setup + run + smoke test (no DB)
  deploy/install.sh     # systemd (from whatswrong, renamed service)
  deploy/deepshit.service
  README.md             # whatswrong-style structure (endpoints replaced)
```

Core deps: `tokio(full)`, `axum`, `reqwest`, `serde/serde_json`,
`clap(derive,env)`, `tracing`, `tower-http(cors)`. No `qrcode`.

Reused from whatswrong (shape, not blind copy-paste):

| From whatswrong | Used for |
|---|---|
| `Archive` JSONL + setup/reset pattern | chat + session archive |
| Bearer auth + `/health` + outgoing webhook | same |
| Session-per-id + per-session queue | session per DeepSeek account |
| systemd unit + install.sh + local-up.sh | deploy + local e2e |
| Watchdog reconnect | refresh-token watchdog |

Not used: Noise/Signal/sqlite-protocol (WA-only), terminal QR.

## 4. Per-crate implementation (whatswrong lessons)

### 4.1 `ds-auth` — mirror `WaSocket`, not a global manager

```rust
pub struct DsConfig { pub base_url: String }   // https://chat.deepseek.com
pub struct DsSocket {                          // ~ WaSocket: 1 account owned by caller
    pub id: String,
    pub http: reqwest::Client,
    pub token: RwLock<Option<String>>,         // Bearer from login
}
impl DsSocket {
    pub async fn login_with_token(id, token, cookies, data_dir) -> Result<Self>
    pub async fn load(id, data_dir) -> Result<Self>               // no re-login
    pub async fn ensure(&self) -> Result<()>  // 401 → re-check once, then error
    pub async fn logout(self) -> Result<()>   // delete session file
}
```

Lesson: sessions must survive restarts without re-login, and every
auth failure → refresh → retry once.

### 4.2 `ds-chat` — one flexible send (mirrors `send_text_ext`)

```rust
pub struct SendOpt { pub thinking: bool, pub search: bool }
pub async fn send(&self, chat_id: &str, text: &str, opt: SendOpt) -> Result<Reply>
pub async fn send_stream(...) -> (impl Stream<Item = String>, oneshot<ResponseId>)
pub async fn stop_with_msg(&self, chat_id: &str, message_id: Option<i64>) -> Result<()>
pub async fn upload(&self, bytes: Vec<u8>, filename: &str) -> Result<String> // -> file_id
pub async fn wait_file_ready(&self, file_id: &str, timeout: u64) -> Result<()>
```

Lesson: upload is ALWAYS two phases (local archive + hash first, then
upload) — and new files are `PENDING`, so poll `fetch_files?file_ids=`
until `SUCCESS` before referencing them.

### 4.3 Gateway — whatswrong's `main.rs` shape, different routes

```text
POST /sessions/login {id,token,cookies?} -> {id,logged_in}
GET  /sessions | GET /sessions/:id/status | POST /sessions/:id/refresh
POST /sessions/:id/chats -> {chat_id}                 (newchat)
GET  /sessions/:id/chats | GET .../chats/:cid/messages | DELETE .../chats/:cid
POST .../chats/:cid/messages {text,thinking?,search?,model?}
POST .../chats/:cid/messages/stream | .../messages/with-files | .../chats/:cid/stop
POST .../chats/:cid/rename | POST .../upload
POST /v1/chat/completions | GET /v1/models           (F6)
```

Plus the proven bits: login guard (`409` + hint), `/health` without auth,
`WEBHOOK_URL` (finished replies), `--session-id ""` manual mode,
`reset --yes`, CORS for browser apps, per-IP rate limiting on `/v1`.

## 5. Risks (honest)

1. **ToS & bans**: web-chat automation may count as abuse → limits/bans.
   Mitigation: client-side rate limits, 1 account = sane use, no sharing.
2. **Fragile login**: CAPTCHA/Turnstile/Google OAuth = no pure-HTTP login;
   token import is the only path (documented, verified).
3. **Web drift**: web endpoints change without notice → watchdog +
   defensive parsers + daily smoke test.
4. **Context & state**: long histories = big tokens; truncate/summarize
   per chat (configurable per request — future work).

## 6. Phases + done criteria ✅ all verified live

- **F0 research** (§1): protocol doc + `curl` login→send→reply script. ✅
- **F1 auth**: API login, stored session, restart without re-login, refresh. ✅
- **F2 core chat**: newchat, send, reply (non-stream first, read on web),
  continue old chat, list, delete (gone on web), rename (synced). ✅
- **F3 streaming + stop**: staged SSE via `curl -N`; stop mid-way (8 KB vs
  48 KB control). ✅
- **F4 features + upload**: thinking split, search citations, rename, JSONL
  archive, webhook; upload → READY → reply quotes file content. ✅
- **F5 deploy**: binary + systemd + install.sh + README + local-up.sh,
  release binary (~6 MB) smoke-tested. ✅
- **F6 OpenAI-compat** (later): `POST /v1/chat/completions` +
  `GET /v1/models`, `messages`→new/continued chat, SSE `data: [DONE]`,
  tested with the real OpenAI SDK against `base_url`. No function calling. ✅
