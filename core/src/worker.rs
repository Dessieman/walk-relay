//! Talking to a group's Walk Relay Worker (see `worker/` in the repo).
//!
//! In a secured group the app never holds the bucket's key. It has a personal
//! token for the Worker, which checks it and only allows Walk Relay's own files.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::store::{normalize_etag, Cond, ObjInfo, PutOutcome, R2Store, RemoteStore};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub owner: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteInfo {
    pub id: String,
    pub url: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
struct Joined {
    token: String,
    member: Member,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

pub struct WorkerClient {
    base: String,
    token: String,
    agent: ureq::Agent,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
}

/// `walkrelay.x.workers.dev`, `https://walkrelay.x.workers.dev/` -> `https://walkrelay.x.workers.dev`
pub fn normalize_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let u = if u.contains("://") { u.to_string() } else { format!("https://{u}") };
    let parsed = url::Url::parse(&u).context("That doesn't look like a web address")?;
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1")) {
        bail!("The Worker address should start with https://");
    }
    let host = parsed.host_str().context("That doesn't look like a web address")?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{}://{host}{port}", parsed.scheme()))
}

fn http_error(what: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<ErrorBody>(&body).map(|b| b.error).unwrap_or(body);
            anyhow!("{what}: {msg} (HTTP {code})")
        }
        ureq::Error::Transport(t) => anyhow!("{what}: network error: {t}"),
    }
}

impl WorkerClient {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        Ok(Self { base: normalize_url(url)?, token: token.to_string(), agent: agent() })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Join with a single-use invite. Returns the personal token.
    pub fn redeem(url: &str, invite_id: &str, name: &str) -> Result<(String, Member)> {
        let base = normalize_url(url)?;
        let resp = agent()
            .post(&format!("{base}/redeem"))
            .send_json(serde_json::json!({ "invite": invite_id, "name": name }))
            .map_err(|e| http_error("Couldn't join", e))?;
        let j: Joined = resp.into_json()?;
        Ok((j.token, j.member))
    }

    /// Become the group's owner by proving we hold the bucket's key: write a
    /// one-time marker straight into the bucket, then ask the Worker to check it.
    pub fn setup(url: &str, direct: &R2Store, name: &str) -> Result<(String, Member)> {
        let base = normalize_url(url)?;
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|e| anyhow!("no randomness available: {e}"))?;
        let nonce = hex::encode(nonce);
        direct.put(&format!("auth/bootstrap/{nonce}"), b"", Cond::None).context("Couldn't write to the bucket")?;
        let resp = agent()
            .post(&format!("{base}/setup"))
            .send_json(serde_json::json!({ "nonce": nonce, "name": name }))
            .map_err(|e| http_error("The Worker didn't accept the setup", e))?;
        let j: Joined = resp.into_json()?;
        Ok((j.token, j.member))
    }

    pub fn me(&self) -> Result<Member> {
        self.agent
            .get(&format!("{}/me", self.base))
            .set("authorization", &self.auth())
            .call()
            .map_err(|e| http_error("Couldn't reach your group", e))?
            .into_json()
            .map_err(Into::into)
    }

    pub fn create_invite(&self) -> Result<InviteInfo> {
        self.agent
            .post(&format!("{}/invites", self.base))
            .set("authorization", &self.auth())
            .call()
            .map_err(|e| http_error("Couldn't create an invite", e))?
            .into_json()
            .map_err(Into::into)
    }

    pub fn members(&self) -> Result<Vec<Member>> {
        self.agent
            .get(&format!("{}/members", self.base))
            .set("authorization", &self.auth())
            .call()
            .map_err(|e| http_error("Couldn't load members", e))?
            .into_json()
            .map_err(Into::into)
    }

    pub fn remove_member(&self, id: &str) -> Result<()> {
        anyhow::ensure!(id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit()), "bad member id");
        self.agent
            .post(&format!("{}/members/{id}/remove", self.base))
            .set("authorization", &self.auth())
            .call()
            .map_err(|e| http_error("Couldn't remove that member", e))?;
        Ok(())
    }

    fn object_url(&self, key: &str) -> String {
        // Walk Relay keys are hex, digits, dots, dashes and slashes: safe in a path.
        format!("{}/o/{key}", self.base)
    }
}

impl RemoteStore for WorkerClient {
    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        #[derive(Deserialize)]
        struct Item {
            key: String,
            etag: String,
            size: u64,
        }
        let items: Vec<Item> = self
            .agent
            .get(&format!("{}/l", self.base))
            .query("prefix", prefix)
            .set("authorization", &self.auth())
            .call()
            .map_err(|e| http_error("list", e))?
            .into_json()?;
        Ok(items
            .into_iter()
            .map(|i| ObjInfo { key: i.key, etag: normalize_etag(&i.etag), size: i.size })
            .collect())
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        match self.agent.get(&self.object_url(key)).set("authorization", &self.auth()).call() {
            Ok(resp) => {
                let etag = normalize_etag(resp.header("etag").unwrap_or(""));
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut resp.into_reader(), &mut data)?;
                Ok(Some((data, etag)))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(http_error(&format!("download {key}"), e)),
        }
    }

    fn put(&self, key: &str, data: &[u8], cond: Cond) -> Result<PutOutcome> {
        let mut req = self.agent.put(&self.object_url(key)).set("authorization", &self.auth());
        match &cond {
            Cond::None => {}
            Cond::IfMatch(e) => req = req.set("if-match", &format!("\"{}\"", normalize_etag(e))),
            Cond::IfNoneMatch => req = req.set("if-none-match", "*"),
        }
        match req.send_bytes(data) {
            Ok(resp) => Ok(PutOutcome::Ok { etag: normalize_etag(resp.header("etag").unwrap_or("")) }),
            Err(ureq::Error::Status(412, _)) => Ok(PutOutcome::PreconditionFailed),
            Err(e) => Err(http_error(&format!("upload {key}"), e)),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.agent.delete(&self.object_url(key)).set("authorization", &self.auth()).call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(http_error(&format!("delete {key}"), e)),
        }
    }

    fn prunes_history(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn worker_urls_normalize() {
        assert_eq!(normalize_url("walkrelay.chris.workers.dev").unwrap(), "https://walkrelay.chris.workers.dev");
        assert_eq!(normalize_url(" https://walkrelay.chris.workers.dev/ ").unwrap(), "https://walkrelay.chris.workers.dev");
        assert_eq!(normalize_url("https://walkrelay.chris.workers.dev/j/ABC").unwrap(), "https://walkrelay.chris.workers.dev");
        assert_eq!(normalize_url("http://127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert!(normalize_url("http://evil.example").is_err());
    }
}
