//! Arsip JSONL append-only per session: `<dir>/<session>.jsonl`.
use async_trait::async_trait;
use ds_core::DsError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMsg {
    pub msg_id: String,
    pub jid: String,
    pub content: serde_json::Value,
    pub ts: u64,
}

#[async_trait]
pub trait Archive: Send + Sync {
    async fn save_message(
        &self,
        session: &str,
        msg_id: &str,
        jid: &str,
        content: serde_json::Value,
    ) -> Result<(), DsError>;
    async fn recent(&self, session: &str, limit: usize) -> Result<Vec<StoredMsg>, DsError>;
    /// Hapus arsip satu session (dipakai reset/delete).
    async fn clear(&self, session: &str) -> Result<(), DsError> {
        let _ = session;
        Ok(())
    }
}

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct JsonlArchive {
    dir: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlArchive {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            write_lock: Mutex::new(()),
        }
    }
    fn path(&self, session: &str) -> PathBuf {
        let safe: String = session
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == ':' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.jsonl"))
    }
}

#[async_trait]
impl Archive for JsonlArchive {
    async fn save_message(
        &self,
        session: &str,
        msg_id: &str,
        jid: &str,
        content: serde_json::Value,
    ) -> Result<(), DsError> {
        let _g = self.write_lock.lock().await;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        let line = serde_json::to_string(&StoredMsg {
            msg_id: msg_id.into(),
            jid: jid.into(),
            content,
            ts: now(),
        })
        .map_err(|e| DsError::Store(e.to_string()))?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(session))
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        use tokio::io::AsyncWriteExt;
        f.write_all(line.as_bytes())
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        f.write_all(b"\n")
            .await
            .map_err(|e| DsError::Store(e.to_string()))?;
        Ok(())
    }

    async fn recent(&self, session: &str, limit: usize) -> Result<Vec<StoredMsg>, DsError> {
        let data = tokio::fs::read_to_string(self.path(session))
            .await
            .unwrap_or_default();
        let mut out: Vec<StoredMsg> = data
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }

    async fn clear(&self, session: &str) -> Result<(), DsError> {
        let _g = self.write_lock.lock().await;
        let p = self.path(session);
        if p.exists() {
            tokio::fs::remove_file(&p)
                .await
                .map_err(|e| DsError::Store(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn jsonl_roundtrip() {
        let dir = std::env::temp_dir().join("ds-archive-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let a = JsonlArchive::new(dir.clone());
        a.save_message("s1", "m1", "chat-1", serde_json::json!({"t":1}))
            .await
            .unwrap();
        a.save_message("s1", "m2", "chat-1", serde_json::json!({"t":2}))
            .await
            .unwrap();
        let r = a.recent("s1", 10).await.unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].msg_id, "m2");
        a.clear("s1").await.unwrap();
        let r2 = a.recent("s1", 10).await.unwrap();
        assert!(r2.is_empty());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
