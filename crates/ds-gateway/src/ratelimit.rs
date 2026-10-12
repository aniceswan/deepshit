//! Rate limiter sliding-window per IP (port server/ratelimit.py sums001).
//! Tanpa Redis — cukup untuk 1 proses. `/health` dikecualikan.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct RateLimiter {
    limit: u32,
    window: Duration,
    inner: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new(limit: u32, window_secs: u64) -> Self {
        Self {
            limit,
            window: Duration::from_secs(window_secs),
            inner: Mutex::new(HashMap::new()),
        }
    }
    /// Catat request. Ok(remaining) bila lolos; Err(retry_after_secs) bila 429.
    pub fn hit(&self, key: &str) -> Result<u32, f64> {
        let now = Instant::now();
        let mut m = self.inner.lock().unwrap();
        let q = m.entry(key.to_string()).or_default();
        while q.front().map(|t| now.duration_since(*t) > self.window).unwrap_or(false) {
            q.pop_front();
        }
        if q.len() as u32 >= self.limit {
            let retry = self.window.as_secs_f64()
                - now.duration_since(*q.front().unwrap()).as_secs_f64();
            return Err(retry.max(0.0));
        }
        q.push_back(now);
        Ok(self.limit - q.len() as u32)
    }
}

pub fn client_key(headers: &axum::http::HeaderMap, fallback: &str) -> String {
    if let Some(fwd) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = fwd.split(',').next() {
            let s = first.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    fallback.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allows_then_blocks() {
        let l = RateLimiter::new(2, 60);
        assert!(l.hit("a").is_ok());
        assert!(l.hit("a").is_ok());
        assert!(l.hit("a").is_err());
        assert!(l.hit("b").is_ok());
    }
}
