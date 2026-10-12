//! Manager session per akun: load malas + serialisasi upstream per akun.

use ds_auth::DsSocket;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, RwLock};

pub struct DsManager {
    data_dir: PathBuf,
    sessions: RwLock<HashMap<String, Arc<DsSocket>>>,
    /// 1 mutex per akun agar request upstream serial (PoW + hindari hammer).
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

impl DsManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            sessions: RwLock::new(HashMap::new()),
            locks: RwLock::new(HashMap::new()),
        }
    }

    pub async fn lock_for(&self, id: &str) -> Arc<Mutex<()>> {
        let mut l = self.locks.write().await;
        l.entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Ambil dari cache atau load file. Error bila belum login.
    pub async fn get(&self, id: &str) -> Result<Arc<DsSocket>, String> {
        if let Some(s) = self.sessions.read().await.get(id) {
            return Ok(s.clone());
        }
        match DsSocket::load(id, &self.data_dir).await {
            Ok(s) => {
                self.sessions.write().await.insert(id.to_string(), s.clone());
                Ok(s)
            },
            Err(e) => Err(e.to_string()),
        }
    }

    pub async fn login_token(
        &self,
        id: &str,
        token: &str,
        cookies: HashMap<String, String>,
    ) -> Result<Arc<DsSocket>, String> {
        let s = DsSocket::login_with_token(id, token, cookies, &self.data_dir)
            .await
            .map_err(|e| e.to_string())?;
        self.sessions.write().await.insert(id.to_string(), s.clone());
        Ok(s)
    }

    pub async fn list(&self) -> Vec<String> {
        let mut out = DsSocket::list_stored(&self.data_dir).await;
        for k in self.sessions.read().await.keys() {
            if !out.contains(k) {
                out.push(k.clone());
            }
        }
        out.sort();
        out
    }

    pub async fn remove(&self, id: &str) {
        self.sessions.write().await.remove(id);
        self.locks.write().await.remove(id);
    }
}
