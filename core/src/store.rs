//! Remote storage: an S3-compatible bucket with conditional writes.
//!
//! The conditional PUTs (`If-Match` / `If-None-Match: *`) are what make the
//! sync safe: the server refuses an upload that was based on a stale copy, so
//! one friend can never silently overwrite another friend's newer progress.

use anyhow::{anyhow, Context, Result};
use rusty_s3::{actions::ListObjectsV2, Bucket, Credentials, S3Action, UrlStyle};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::GroupCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    /// Unconditional write.
    None,
    /// Only write if the object currently has this ETag.
    IfMatch(String),
    /// Only write if the object does not exist yet.
    IfNoneMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Ok { etag: String },
    /// Someone else changed the object first.
    PreconditionFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjInfo {
    pub key: String,
    pub etag: String,
    pub size: u64,
}

pub trait RemoteStore: Send + Sync {
    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>>;
    /// Returns `(bytes, etag)` or `None` if missing.
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>>;
    fn put(&self, key: &str, data: &[u8], cond: Cond) -> Result<PutOutcome>;
    fn delete(&self, key: &str) -> Result<()>;
}

fn normalize_etag(e: &str) -> String {
    e.trim().trim_start_matches("W/").trim_matches('"').to_string()
}

// ---------------------------------------------------------------- in-memory

/// A faithful in-memory store for tests (same conditional semantics as R2).
#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<(u64, BTreeMap<String, (Vec<u8>, String)>)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().1.keys().cloned().collect()
    }
}

impl RemoteStore for MemoryStore {
    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        let g = self.inner.lock().unwrap();
        Ok(g.1
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, (d, e))| ObjInfo { key: k.clone(), etag: e.clone(), size: d.len() as u64 })
            .collect())
    }
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        Ok(self.inner.lock().unwrap().1.get(key).cloned())
    }
    fn put(&self, key: &str, data: &[u8], cond: Cond) -> Result<PutOutcome> {
        let mut g = self.inner.lock().unwrap();
        let current = g.1.get(key).map(|(_, e)| e.clone());
        let ok = match (&cond, &current) {
            (Cond::None, _) => true,
            (Cond::IfNoneMatch, None) => true,
            (Cond::IfNoneMatch, Some(_)) => false,
            (Cond::IfMatch(want), Some(have)) => want == have,
            (Cond::IfMatch(_), None) => false,
        };
        if !ok {
            return Ok(PutOutcome::PreconditionFailed);
        }
        g.0 += 1;
        let etag = format!("etag-{}", g.0);
        g.1.insert(key.to_string(), (data.to_vec(), etag.clone()));
        Ok(PutOutcome::Ok { etag })
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.inner.lock().unwrap().1.remove(key);
        Ok(())
    }
}

// ---------------------------------------------------------------- R2 / S3

pub struct R2Store {
    bucket: Bucket,
    creds: Credentials,
    agent: ureq::Agent,
}

const SIGN_FOR: Duration = Duration::from_secs(300);

impl R2Store {
    pub fn new(group: &GroupCode) -> Result<Self> {
        let endpoint = url::Url::parse(group.endpoint.trim()).context("bad endpoint URL")?;
        let bucket = Bucket::new(endpoint, UrlStyle::Path, group.bucket.trim().to_string(), group.region.clone())
            .map_err(|e| anyhow!("bad bucket settings: {e}"))?;
        let creds = Credentials::new(group.key_id.trim(), group.secret.trim());
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build();
        Ok(Self { bucket, creds, agent })
    }

    /// Write, read and delete a scratch object to prove the credentials work.
    pub fn check_access(&self) -> Result<()> {
        let key = format!("v1/_check/{}", crate::now_ms());
        match self.put(&key, b"ok", Cond::IfNoneMatch)? {
            PutOutcome::Ok { .. } => {}
            PutOutcome::PreconditionFailed => {}
        }
        let got = self.get(&key)?.context("wrote a test file but could not read it back")?;
        if got.0 != b"ok" {
            return Err(anyhow!("read back the wrong data"));
        }
        self.delete(&key)?;
        Ok(())
    }
}

fn http_err(op: &str, key: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let hint = match code {
                401 | 403 => " (check the access key / secret in the group code)",
                404 => " (check the bucket name)",
                _ => "",
            };
            anyhow!("{op} {key}: HTTP {code}{hint}: {}", body.chars().take(300).collect::<String>())
        }
        ureq::Error::Transport(t) => anyhow!("{op} {key}: network error: {t}"),
    }
}

impl RemoteStore for R2Store {
    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = ListObjectsV2::new(&self.bucket, Some(&self.creds));
            action.with_prefix(prefix);
            if let Some(t) = &token {
                action.with_continuation_token(t.clone());
            }
            let url = action.sign(SIGN_FOR);
            let body = self
                .agent
                .get(url.as_str())
                .call()
                .map_err(|e| http_err("list", prefix, e))?
                .into_string()?;
            let parsed = ListObjectsV2::parse_response(&body).map_err(|e| anyhow!("bad list response: {e}"))?;
            for c in parsed.contents {
                out.push(ObjInfo {
                    etag: normalize_etag(c.etag.as_deref().unwrap_or("")),
                    key: c.key,
                    size: c.size.unwrap_or(0),
                });
            }
            match parsed.next_continuation_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        let url = self.bucket.get_object(Some(&self.creds), key).sign(SIGN_FOR);
        match self.agent.get(url.as_str()).call() {
            Ok(resp) => {
                let etag = normalize_etag(resp.header("etag").unwrap_or(""));
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut resp.into_reader(), &mut data)?;
                Ok(Some((data, etag)))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(http_err("download", key, e)),
        }
    }

    fn put(&self, key: &str, data: &[u8], cond: Cond) -> Result<PutOutcome> {
        let mut action = self.bucket.put_object(Some(&self.creds), key);
        let header = match &cond {
            Cond::None => None,
            Cond::IfMatch(e) => Some(("if-match", format!("\"{}\"", normalize_etag(e)))),
            Cond::IfNoneMatch => Some(("if-none-match", "*".to_string())),
        };
        if let Some((name, value)) = &header {
            action.headers_mut().insert(*name, value.clone());
        }
        let url = action.sign(SIGN_FOR);
        let mut req = self.agent.put(url.as_str());
        if let Some((name, value)) = &header {
            req = req.set(name, value);
        }
        match req.send_bytes(data) {
            Ok(resp) => Ok(PutOutcome::Ok { etag: normalize_etag(resp.header("etag").unwrap_or("")) }),
            // 412: condition failed. 409: concurrent conditional write in flight.
            Err(ureq::Error::Status(412, _)) | Err(ureq::Error::Status(409, _)) => Ok(PutOutcome::PreconditionFailed),
            Err(ureq::Error::Status(404, _)) if matches!(cond, Cond::IfMatch(_)) => Ok(PutOutcome::PreconditionFailed),
            Err(e) => Err(http_err("upload", key, e)),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        let url = self.bucket.delete_object(Some(&self.creds), key).sign(SIGN_FOR);
        match self.agent.delete(url.as_str()).call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(http_err("delete", key, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_conditional_semantics() {
        let s = MemoryStore::new();
        let PutOutcome::Ok { etag: e1 } = s.put("a", b"1", Cond::IfNoneMatch).unwrap() else { panic!() };
        assert_eq!(s.put("a", b"2", Cond::IfNoneMatch).unwrap(), PutOutcome::PreconditionFailed);
        let PutOutcome::Ok { etag: e2 } = s.put("a", b"2", Cond::IfMatch(e1.clone())).unwrap() else { panic!() };
        assert_eq!(s.put("a", b"3", Cond::IfMatch(e1)).unwrap(), PutOutcome::PreconditionFailed);
        assert_eq!(s.get("a").unwrap().unwrap(), (b"2".to_vec(), e2));
        assert_eq!(s.list("a").unwrap().len(), 1);
        assert_eq!(s.list("b").unwrap().len(), 0);
    }

    #[test]
    fn etags_normalize() {
        assert_eq!(normalize_etag("\"abc\""), "abc");
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
    }
}
