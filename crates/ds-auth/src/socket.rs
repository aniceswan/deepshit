use ds_core::{constants::*, types::*, DsError};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct DsConfig {
    pub base_url: String,
}

impl Default for DsConfig {
    fn default() -> Self {
        Self {
            base_url: BASE_URL.into(),
        }
    }
}

/// 1 akun milik pemanggil: HTTP client + token + cookies, tersimpan di file.
pub struct DsSocket {
    pub id: String,
    pub cfg: DsConfig,
    pub http: reqwest::Client,
    data_dir: PathBuf,
    token: RwLock<Option<String>>,
    cookies: RwLock<HashMap<String, String>>,
    user_agent: RwLock<String>,
}

impl DsSocket {
    fn session_path(id: &str, data_dir: &Path) -> PathBuf {
        data_dir.join(id).join("session.json")
    }

    fn default_ua() -> String {
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36".into()
    }

    fn build_client() -> Result<reqwest::Client, DsError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| DsError::Transport(e.to_string()))
    }

    /// Login via token yang sudah di-capture (jalur utama, F1).
    /// `cookies`: e.g. {"aws-waf-token": "...", ...} — may be empty.
    pub async fn login_with_token(
        id: &str,
        token: &str,
        cookies: HashMap<String, String>,
        data_dir: &Path,
    ) -> Result<Arc<Self>, DsError> {
        let token = token.trim().trim_start_matches("Bearer ").to_string();
        if token.is_empty() {
            return Err(DsError::Invalid("token is empty".into()));
        }
        let s = Arc::new(Self {
            id: id.into(),
            cfg: DsConfig::default(),
            http: Self::build_client()?,
            data_dir: data_dir.to_path_buf(),
            token: RwLock::new(Some(token.clone())),
            cookies: RwLock::new(cookies.clone()),
            user_agent: RwLock::new(Self::default_ua()),
        });
        // Validasi ringan: coba /users/current. Bila 401 -> token salah.
        // Bila network/WAF gagal -> tetap simpan (offline-first), error dijelaskan.
        match s.check_current().await {
            Ok(_) => {},
            Err(DsError::NotAuthenticated(_)) => {
                return Err(DsError::NotAuthenticated(
                    "token rejected by server (401). Copy a fresh one from chat.deepseek.com (DevTools -> localStorage userToken / authorization header)".into(),
                ));
            },
            Err(_) => {
                // Simpan saja; validasi menyusul saat kirim pertama.
            },
        }
        s.save().await?;
        Ok(s)
    }

    /// Muat session tersimpan tanpa login ulang (syarat F1).
    pub async fn load(id: &str, data_dir: &Path) -> Result<Arc<Self>, DsError> {
        let p = Self::session_path(id, data_dir);
        let raw = tokio::fs::read_to_string(&p)
            .await
            .map_err(|_| DsError::LoginRequired(format!("session '{id}' does not exist, POST /sessions/login first")))?;
        let sess: Session =
            serde_json::from_str(&raw).map_err(|e| DsError::Store(e.to_string()))?;
        Ok(Arc::new(Self {
            id: id.into(),
            cfg: DsConfig::default(),
            http: Self::build_client()?,
            data_dir: data_dir.to_path_buf(),
            token: RwLock::new(Some(sess.token)),
            cookies: RwLock::new(sess.cookies),
            user_agent: RwLock::new(sess.user_agent),
        }))
    }

    /// Daftar session yang punya file tersimpan.
    pub async fn list_stored(data_dir: &Path) -> Vec<String> {
        let mut out = vec![];
        let mut rd = match tokio::fs::read_dir(data_dir).await {
            Ok(r) => r,
            Err(_) => return out,
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.path().join("session.json").exists() {
                if let Some(n) = e.file_name().to_str() {
                    out.push(n.to_string());
                }
            }
        }
        out.sort();
        out
    }

    async fn save(&self) -> Result<(), DsError> {
        let sess = Session {
            token: self.token.read().await.clone().unwrap_or_default(),
            cookies: self.cookies.read().await.clone(),
            user_agent: self.user_agent.read().await.clone(),
            captured_at: now_secs(),
        };
        let dir = self.data_dir.join(&self.id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        let tmp = dir.join("session.json.tmp");
        let dst = dir.join("session.json");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(&sess).unwrap())
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        tokio::fs::rename(&tmp, &dst)
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        Ok(())
    }

    pub async fn token(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    pub async fn is_logged_in(&self) -> bool {
        self.token.read().await.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
    }

    /// Header auth + browser untuk tiap request DeepSeek.
    pub async fn base_headers(&self) -> Result<HeaderMap, DsError> {
        let tok = self
            .token
            .read()
            .await
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| DsError::NotAuthenticated("not logged in".into()))?;
        let cookies = self.cookies.read().await.clone();
        let ua = self.user_agent.read().await.clone();
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {tok}"))
                .map_err(|e| DsError::Invalid(e.to_string()))?,
        );
        h.insert(ACCEPT, HeaderValue::from_static("*/*"));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(USER_AGENT, HeaderValue::from_str(&ua).unwrap_or(HeaderValue::from_static("deepshit/0.1")));
        h.insert(ORIGIN, HeaderValue::from_str(BASE_URL).unwrap());
        h.insert(REFERER, HeaderValue::from_str(&format!("{BASE_URL}/")).unwrap());
        h.insert("x-app-version", HeaderValue::from_static(X_APP_VERSION));
        h.insert("x-client-version", HeaderValue::from_static(X_CLIENT_VERSION));
        h.insert("x-client-platform", HeaderValue::from_static(X_CLIENT_PLATFORM));
        h.insert("x-client-locale", HeaderValue::from_static(X_CLIENT_LOCALE));
        h.insert("x-client-bundle-id", HeaderValue::from_static("com.deepseek.chat"));
        if !cookies.is_empty() {
            let c = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
            if let Ok(v) = HeaderValue::from_str(&c) {
                h.insert(COOKIE, v);
            }
        }
        Ok(h)
    }

    /// Cek token ke /users/current. Ok = valid; 401 = mati.
    pub async fn check_current(&self) -> Result<(), DsError> {
        let headers = self.base_headers().await?;
        let url = format!("{}{}", BASE_URL, PATH_CURRENT_USER);
        // Endpoint ini GET di beberapa wrapper, POST di lainnya — coba GET dulu.
        let r = self.http.get(&url).headers(headers).send().await.map_err(|e| DsError::Transport(e.to_string()))?;
        match r.status().as_u16() {
            401 | 403 => Err(DsError::NotAuthenticated("token expired/invalid (401)".into())),
            200 => {
                let text = r.text().await.unwrap_or_default();
                // Validate the {"code":0,...} envelope: code!=0 means the token was rejected
                // (contoh: code 40003 "Authorization Failed" untuk token dummy).
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
                        if code != 0 {
                            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("auth failed");
                            return Err(DsError::NotAuthenticated(format!(
                                "token rejected by server (code {code}: {msg})"
                            )));
                        }
                        return Ok(());
                    }
                }
                // 200 without a code envelope (or HTML?) — treat as transport so that
                // login dummy tidak lolos diam-diam; pesan jelas.
                if text.trim_start().starts_with('<') {
                    return Err(DsError::Transport("users/current returned HTML (WAF?)".into()));
                }
                Ok(())
            },
            c => {
                let t = r.text().await.unwrap_or_default();
                if c >= 500 {
                    Err(DsError::Transport(format!("users/current HTTP {c}: {}", t.chars().take(200).collect::<String>())))
                } else {
                    // 404/405 (wrong method) does not prove a dead token — treat as ok
                    // agar tidak memblokir, validasi final tetap di completion.
                    Ok(())
                }
            },
        }
    }

    /// Pastikan auth masih hidup; bila 401 coba refresh sekali (users/current),
    /// lalu error jelas bila tetap mati (pola watchdog whatswrong).
    pub async fn ensure(&self) -> Result<(), DsError> {
        if !self.is_logged_in().await {
            return Err(DsError::NotAuthenticated("not logged in".into()));
        }
        match self.check_current().await {
            Ok(_) => Ok(()),
            Err(DsError::NotAuthenticated(_)) => {
                // Satu kali percobaan ulang (token bisa di-refresh browser; di sini
                // we only re-verify — a full refresh needs a browser).
                match self.check_current().await {
                    Ok(_) => Ok(()),
                    Err(e) => Err(DsError::NotAuthenticated(format!(
                        "{e}. Copy a fresh token from the browser, then POST /sessions/login again"
                    ))),
                }
            },
            Err(e) => Err(e),
        }
    }

    /// Refresh: tulis ulang file (umur baru) bila token masih valid.
    pub async fn refresh(&self) -> Result<(), DsError> {
        self.check_current().await?;
        self.save().await
    }

    pub async fn logout(self: Arc<Self>) -> Result<(), DsError> {
        let dir = self.data_dir.join(&self.id);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await.map_err(|e| DsError::Store(e.to_string()))?;
        }
        Ok(())
    }

    pub fn data_subdir(&self, name: &str) -> PathBuf {
        self.data_dir.join(&self.id).join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join("ds-auth-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let mut c = HashMap::new();
        c.insert("aws-waf-token".into(), "abc".into());
        // login_with_token mencoba network; pakai konstruksi manual untuk unit murni.
        let s = DsSocket {
            id: "u1".into(),
            cfg: DsConfig::default(),
            http: DsSocket::build_client().unwrap(),
            data_dir: dir.clone(),
            token: RwLock::new(Some("tok123".into())),
            cookies: RwLock::new(c),
            user_agent: RwLock::new(DsSocket::default_ua()),
        };
        s.save().await.unwrap();
        let l = DsSocket::load("u1", &dir).await.unwrap();
        assert_eq!(l.token().await.unwrap(), "tok123");
        assert!(l.is_logged_in().await);
        let h = l.base_headers().await.unwrap();
        assert!(h.contains_key(reqwest::header::AUTHORIZATION));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
