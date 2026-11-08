use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CODE_PREFIX: &str = "WR1-";

/// Everything needed to reach the group's shared bucket. Shared between
/// friends as a single copy-pasteable "group code".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCode {
    /// e.g. `https://<account-id>.r2.cloudflarestorage.com`
    pub endpoint: String,
    pub bucket: String,
    pub key_id: String,
    pub secret: String,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    "auto".into()
}

impl GroupCode {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(code: &str) -> Result<Self> {
        let cleaned: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let body = cleaned
            .strip_prefix(CODE_PREFIX)
            .context("That doesn't look like a Walk Relay group code (it should start with WR1-)")?;
        let json = URL_SAFE_NO_PAD
            .decode(body.trim_end_matches('='))
            .context("The group code is damaged - copy it again")?;
        let code: GroupCode =
            serde_json::from_slice(&json).context("The group code is damaged - copy it again")?;
        code.validate()?;
        Ok(code)
    }

    /// Short stable id for this group's bucket (used to keep per-group sync state apart).
    pub fn id(&self) -> String {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(format!("{}|{}", self.endpoint.trim().trim_end_matches('/'), self.bucket.trim()).as_bytes());
        hex::encode(&h[..8])
    }

    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(self.endpoint.trim()).context("Endpoint must be a URL")?;
        if url.scheme() != "https" && url.scheme() != "http" {
            bail!("Endpoint must start with https://");
        }
        if !matches!(url.path(), "" | "/") || url.query().is_some() {
            bail!("The endpoint should be just https://<account-id>.r2.cloudflarestorage.com (without the bucket name or anything after it)");
        }
        if self.bucket.trim().is_empty() || self.key_id.trim().is_empty() || self.secret.trim().is_empty() {
            bail!("Bucket, access key ID and secret are all required");
        }
        Ok(())
    }
}

/// Per-machine settings, stored as `config.json` in the data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub group: GroupCode,
    pub player_name: String,
    /// Extra save folders to sync in addition to the auto-detected ones.
    #[serde(default)]
    pub extra_save_dirs: Vec<PathBuf>,
}

impl AppConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(data_dir);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        crate::engine::write_atomic(&Self::path(data_dir), &serde_json::to_vec_pretty(self)?)
    }
}

/// Where Walk Relay keeps its own config, sync state and local backups.
pub fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("Walk Relay")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_code_round_trips_and_tolerates_whitespace() {
        let g = GroupCode {
            endpoint: "https://abc.r2.cloudflarestorage.com".into(),
            bucket: "big-walk".into(),
            key_id: "KEY".into(),
            secret: "SECRET".into(),
            region: "auto".into(),
        };
        let code = g.encode();
        assert!(code.starts_with("WR1-"));
        let messy = format!("  {}\n{} ", &code[..10], &code[10..]);
        assert_eq!(GroupCode::decode(&messy).unwrap(), g);
        assert!(GroupCode::decode("hello").is_err());
    }
}
