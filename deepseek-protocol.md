# DeepSeek Web Protocol (F0) — chat.deepseek.com

> Phase-0 output (§1 of PLAN-deepseek.md): endpoint list + raw
> request/response samples (sanitized, no credentials). Researched via
> DevTools + reference projects `sums001/Deepseek-API`, `deepseek2api`,
> `dskpp`, `redeflesq/deepseek-pow`, `Brhiza/ds2api`, `dyunwei/ds2api`.
> PoW logic verified against the official WASM vectors in `ds-core`,
> then confirmed live against a real account.

Backend: **`https://chat.deepseek.com`** (free web chat).
Not `api.deepseek.com` (paid, official OpenAI-compatible API).
Internal API prefix: **`/api/v0`**.

## 1. Login flow (honest answer)

| Method | Status |
|---|---|
| Email+password over plain HTTP (`POST /api/v0/users/login {email,password,device_id,os}`) | **Dead** on the modern site. The legacy endpoint still exists but fails (WAF/Turnstile/CAPTCHA, Google OAuth). Verified: even a real browser gets `202 WAF challenge` → `403 CloudFront blocked`. This gateway does not offer it — token import only. |
| Browser + token import (the real path) | **Works**. Login manually once, copy `userToken` + cookies, import via `POST /sessions/login`. |
| `python -m deepseek.auth` (Playwright, sums001) | Reference approach: persistent Chromium, solve the human-check once, capture token+cookies to `session/session.json`, headless refresh ~6h. |

### Session artifacts

- **Bearer**: `localStorage.userToken = {"value":"<TOKEN>","__version":"0"}` → header `Authorization: Bearer <TOKEN>` on every request.
- **Cookies**: at minimum `aws-waf-token` (earned via the AWS WAF human-check); ideally the whole browser jar (`cf_clearance`, `session`, …). Sent as the `Cookie` header.
- **Mandatory browser headers**:
  ```
  accept: */*
  content-type: application/json
  origin: https://chat.deepseek.com
  referer: https://chat.deepseek.com/
  user-agent: <real browser UA>
  x-app-version: 2.0.0
  x-client-version: 2.0.0
  x-client-platform: web
  x-client-locale: en_US
  x-client-bundle-id: com.deepseek.chat
  ```
- **Lifetime**: long-lived token (hours–days); WAF cookies shorter. Refresh check via `GET /api/v0/users/current` (200 + `code:0` = alive, 401/403 or auth `code!=0` = dead → re-import). Gateway: `POST /sessions/:id/refresh` + 5-minute watchdog.
- **Copy it manually**:
  1. Open `chat.deepseek.com`, login.
  2. F12 → Application → Local Storage → `https://chat.deepseek.com` → `userToken` → copy `value`.
  3. Or Network → click any `/api/v0/...` request → copy `authorization: Bearer ...` + `cookie`.

## 2. Chat endpoints

### 2.1 New chat

```http
POST /api/v0/chat_session/create
{}
→ {"code":0,"data":{"biz_data":{"chat_session":{"id":"<uuid>"}}}}
```

### 2.2 PoW challenge (required before completion/upload)

```http
POST /api/v0/chat/create_pow_challenge
{"target_path":"/api/v0/chat/completion"}
→ {"code":0,"data":{"biz_data":{"challenge":{
  "algorithm":"DeepSeekHashV1",
  "challenge":"b4904764f21e49261b7e8aa1cca16f32451d54ebcd65d13037083f0c4b4d31b0",
  "salt":"865fc16c1f6c7b648547",
  "signature":"b5625327fb6c08b4e96d05bd04628edbcca3d445b3c1a479b86069affd506796",
  "difficulty":144000,
  "expire_at":1784018472563,
  "expire_after":300000,
  "target_path":"/api/v0/chat/completion"}}}}
```

For upload: `{"target_path":"/api/v0/file/upload_file"}`.

### 2.3 Solving PoW (DeepSeekHashV1)

- `prefix = f"{salt}_{expire_at}_"`
- Find `n ∈ [0, difficulty)` with `DeepSeekHashV1(prefix + str(n)) == challenge`.
- `DeepSeekHashV1 = SHA3-256` with **Keccak-f[1600] rounds 1..23 only** (round 0 skipped). Rate 136, padding `0x06…0x80`, 32-byte output. Standard SHA3 libraries give the **wrong** answer — custom implementation required (verified against the official WASM vectors in `ds-core::pow`; note the round-constant table typo that breaks naive ports: `RC[21]` must be `0x8000000000008080`).
- Header: `x-ds-pow-response: base64(JSON{algorithm,challenge,salt,answer,signature,target_path})` (no difficulty/expire_at).
- Typical `difficulty=144000` (~50–500 ms CPU per message in single-threaded Rust).
- Test vectors (from the official WASM):
  ```
  "" → e594808bc5b7151ac160c6d39a02e0a8e261ed588578403099e3561dc40c26b3
  "testsalt_1700000000_42" → d4a2ea58c89e40887c933484868380c6f803eaa8dc53a3b9df8e431b921a4f09
  ```

### 2.4 Send + streaming

```http
POST /api/v0/chat/completion
x-ds-pow-response: <base64>
{
  "chat_session_id":"<uuid>",
  "parent_message_id": null,      // or int (continue a thread)
  "prompt":"hello",
  "ref_file_ids":[],
  "thinking_enabled": false,      // DeepThink (orthogonal, not a model)
  "search_enabled": false,        // web search (can't combine with upload)
  "model_type":"default",         // "default" (Instant) | "expert"; omit on resume
  "action": null,
  "preempt": false
}
→ 200 text/event-stream, `data: {...}` lines:
```

- Snapshot: `{"v":{"response":{"message_id":2,"fragments":[{"type":"RESPONSE","content":"Hi"}]}}}` — note: id **and** content arrive together (early parsers that return on the id lose the text).
- Pathed append: `{"p":"response/fragments/-1/content","o":"APPEND","v":" there"}`
- Bare append: `{"v":"!"}`
- Thinking mode: snapshot fragment is `{"type":"THINK","content":"We"}` (not `THINKING`), appends stream reasoning until `{"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":"<final>"}]}` delivers the real answer.
- Done: `{"p":"response/status","v":"FINISHED"}`, then `data: [DONE]`
- The assistant `message_id` hides in several spots (defensive check: `response.message_id`, `response.id`, `*.message_id`, `request/response_message_id`) → `conversation_id = "<session>:<msg_id>"`.

### 2.5 Stop generation

```http
POST /api/v0/chat/stop_stream
{"chat_session_id":"<uuid>","message_id":<response_message_id>}
```

`message_id` is required to hit the right stream (dskpp + verified live:
stopped stream ended at 8 KB + `[DONE]`, unstopped control reached 48 KB).
The gateway tracks it automatically from the first stream frame.

### 2.6 List / history / delete / rename

```http
GET /api/v0/chat_session/fetch_page[?lte_cursor.pinned=false]
→ {"code":0,"data":{"biz_data":{"chat_sessions":[{"id":"...","title":"..."}]}}}
```

> Note: `fetch_page` is **GET**, not POST (POST returns `405`).

```http
GET /api/v0/chat/history_messages?chat_session_id=<uuid>
→ {"code":0,"data":{"biz_data":{
  "chat_session":{...},
  "chat_messages":[
    {"message_id":1,"role":"USER","fragments":[{"type":"REQUEST","content":"..."}]},
    {"message_id":2,"role":"ASSISTANT","fragments":[{"type":"RESPONSE","content":"..."}]}]}}}

POST /api/v0/chat_session/delete
{"chat_session_id":"<uuid>"}

POST /api/v0/chat_session/update_title
{"chat_session_id":"<uuid>","title":"..."}
→ {"code":0,"data":{"biz_code":0,"biz_data":{"title":"...","chat_session_updated_at":...}}}
```

Rename notes: empty chats are rejected with `biz_code 5 EMPTY_CHAT_SESSION` — send 1 message first. Found by replaying the UI's `...` → `Rename` clicks (verified live).

### 2.7 File upload

```http
POST /api/v0/file/upload_file
x-ds-pow-response: <base64 for target_path=/api/v0/file/upload_file>
Content-Type: multipart/form-data; file=@...
→ {"code":0,"data":{"biz_data":{"id":"file-...","status":"PENDING","model_kind":"VISION",...}}}
```

A fresh file is `PENDING` → poll until `SUCCESS` (verified live: ~3 min for a
142-byte txt) before referencing it, otherwise completion answers
`biz_code 9 invalid ref file id`:

```http
GET /api/v0/file/fetch_files?file_ids=<id>   (plural param, no PoW needed)
→ {"code":0,"data":{"biz_data":{"files":[{"id":"...","status":"SUCCESS",...}]}}}
```

Rules: `search_enabled` + `ref_file_ids` can't be combined. ~20 MB gateway cap.

## 3. Limits & failure behavior

| Case | Response | Gateway behavior |
|---|---|---|
| Dead token | 401/403, or auth `code!=0` | 409 + re-import hint; watchdog logs |
| WAF/Cloudflare | HTML "Just a moment" / 403 | 502 + refresh-cookies-via-browser advice |
| Rate limit | 429 | 502/429 + `Retry-After`; per-account serial + `/v1` sliding window (30/min) |
| PoW unsolved | solver None (stale challenge) | fetch a fresh challenge once, then error |
| App-level rejection | HTTP 200 JSON envelope (`biz_code!=0`) | surfaced as `Api` error, never silent-empty |
| Endpoint drift | unfamiliar JSON shape | defensive parsers (many fallbacks); log + 502 with 300-char excerpt |

## 4. Raw curl script (F0: login → send → reply)

```bash
# 1. Token from the browser into env (no credentials in the repo!)
TOKEN='<userToken from localStorage>'
COOKIES='aws-waf-token=...; ...'

# 2. New chat
curl -s https://chat.deepseek.com/api/v0/chat_session/create \
  -H "authorization: Bearer $TOKEN" -H "cookie: $COOKIES" \
  -H 'content-type: application/json' -d '{}'
# → {"code":0,"data":{"biz_data":{"chat_session":{"id":"SID"}}}}
SID='<id from above>'

# 3. PoW challenge
curl -s https://chat.deepseek.com/api/v0/chat/create_pow_challenge \
  -H "authorization: Bearer $TOKEN" -H "cookie: $COOKIES" \
  -H 'content-type: application/json' \
  -d '{"target_path":"/api/v0/chat/completion"}'
# → challenge {salt, expire_at, difficulty, ...}; solve with deepshit (ds-core::pow)
# POW='<base64 solver header>'

# 4. Streaming completion
curl -N https://chat.deepseek.com/api/v0/chat/completion \
  -H "authorization: Bearer $TOKEN" -H "cookie: $COOKIES" \
  -H 'content-type: application/json' -H "x-ds-pow-response: $POW" \
  -d "{\"chat_session_id\":\"$SID\",\"parent_message_id\":null,\"prompt\":\"hello\",\"ref_file_ids\":[],\"thinking_enabled\":false,\"search_enabled\":false}"
# → data: {...} ... data: [DONE]
```

> F0–F6 were unit-tested with mocks (PoW vs WASM vectors, SSE parser,
> OpenAI format, archive) and then verified end-to-end against a real
> account: newchat visible on the web, replies, threads, streaming, stop,
> thinking split, search citations, upload→READY→quoted file content,
> rename synced, delete gone, webhook received.
