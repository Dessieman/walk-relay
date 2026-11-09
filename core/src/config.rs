use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Original format: the group's settings as base64url JSON. Works for any S3 storage.
const CODE_PREFIX: &str = "WR1-";
/// Compact format for standard Cloudflare R2 groups: about a third of the length.
const COMPACT_PREFIX: &str = "WR2-";
const COMPACT_VERSION: u8 = 1;
const R2_HOST_SUFFIX: &str = ".r2.cloudflarestorage.com";

/// Web page that turns an invite link into "open Walk Relay". The group code
/// travels in the `#fragment`, which browsers never send to the server.
pub const INVITE_PAGE: &str = "https://dessieman.github.io/walk-relay/join/";
/// Custom URL scheme the app registers, e.g. `walkrelay://join#WR1-...`.
pub const URL_SCHEME: &str = "walkrelay";

/// Pull a group code out of anything a friend might paste or click: the raw
/// code, a `walkrelay://` link, the https invite link, or a whole invite
/// message containing one of those. Returns the code only if it decodes.
pub fn code_from_invite(text: &str) -> Option<String> {
    for prefix in [COMPACT_PREFIX, CODE_PREFIX] {
        let mut from = 0;
        while let Some(i) = text[from..].find(prefix) {
            let rest = &text[from + i..];
            // A code is base64url after the prefix: stop at the first other character
            // (a space, quote, bracket, or the end of a sentence).
            let end = rest[prefix.len()..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .map_or(rest.len(), |j| j + prefix.len());
            let code = &rest[..end];
            if GroupCode::decode(code).is_ok() {
                return Some(code.to_string());
            }
            from += i + prefix.len();
        }
    }
    None
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The friendly message to paste in Discord: the link shows as clickable text
/// (the `<...>` stops Discord from adding a large preview under it).
pub fn discord_invite_message(code: &str) -> String {
    format!(
        "\u{1F97E} [Join our Big Walk group on Walk Relay](<{}>) (keep this private: it unlocks our shared saves)",
        invite_link(code)
    )
}

/// The shareable https invite link for a group code.
pub fn invite_link(code: &str) -> String {
    format!("{INVITE_PAGE}#{code}")
}

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
    /// The shareable code: compact `WR2-` for standard R2 groups, `WR1-` otherwise.
    pub fn encode(&self) -> String {
        if let Some(c) = self.encode_compact() {
            return c;
        }
        let json = serde_json::to_vec(self).expect("serializable");
        format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// `[version][16 B account id][16 B key id][32 B secret][bucket]`, if this
    /// group has exactly the standard R2 shape (so decoding rebuilds it byte for byte).
    fn encode_compact(&self) -> Option<String> {
        let account = self.endpoint.strip_prefix("https://")?.strip_suffix(R2_HOST_SUFFIX)?;
        if !is_lower_hex(account, 32)
            || !is_lower_hex(&self.key_id, 32)
            || !is_lower_hex(&self.secret, 64)
            || self.region != "auto"
            || self.bucket.is_empty()
            || self.bucket.trim() != self.bucket
        {
            return None;
        }
        let mut raw = vec![COMPACT_VERSION];
        raw.extend(hex::decode(account).ok()?);
        raw.extend(hex::decode(&self.key_id).ok()?);
        raw.extend(hex::decode(&self.secret).ok()?);
        raw.extend(self.bucket.as_bytes());
        Some(format!("{COMPACT_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
    }

    pub fn decode(code: &str) -> Result<Self> {
        let cleaned: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let damaged = "The group code is damaged - copy it again";
        let code = if let Some(body) = cleaned.strip_prefix(COMPACT_PREFIX) {
            let raw = URL_SAFE_NO_PAD.decode(body.trim_end_matches('=')).context(damaged)?;
            if raw.len() < 66 || raw[0] != COMPACT_VERSION {
                bail!("{damaged}");
            }
            GroupCode {
                endpoint: format!("https://{}{R2_HOST_SUFFIX}", hex::encode(&raw[1..17])),
                key_id: hex::encode(&raw[17..33]),
                secret: hex::encode(&raw[33..65]),
                bucket: String::from_utf8(raw[65..].to_vec()).context(damaged)?,
                region: "auto".into(),
            }
        } else {
            let body = cleaned
                .strip_prefix(CODE_PREFIX)
                .context("That doesn't look like a Walk Relay group code (it should start with WR2- or WR1-)")?;
            let json = URL_SAFE_NO_PAD.decode(body.trim_end_matches('=')).context(damaged)?;
            serde_json::from_slice(&json).context(damaged)?
        };
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

    fn r2_group() -> GroupCode {
        GroupCode {
            endpoint: "https://88805d02ccb9f4b0678d307e822de210.r2.cloudflarestorage.com".into(),
            bucket: "big-walk".into(),
            key_id: "0123456789abcdef0123456789abcdef".into(),
            secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
            region: "auto".into(),
        }
    }

    #[test]
    fn standard_r2_groups_get_a_compact_code() {
        let g = r2_group();
        let code = g.encode();
        assert!(code.starts_with("WR2-"), "{code}");
        assert!(code.len() <= 104, "{} chars", code.len());
        assert_eq!(GroupCode::decode(&code).unwrap(), g);
        // Old-style codes for the same group still decode, to the same group (and same id).
        let json = format!("WR1-{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&g).unwrap()));
        let old = GroupCode::decode(&json).unwrap();
        assert_eq!(old, g);
        assert_eq!(old.id(), GroupCode::decode(&code).unwrap().id());
    }

    #[test]
    fn other_storage_falls_back_to_the_full_code() {
        for g in [
            GroupCode { endpoint: "https://s3.example.com".into(), ..r2_group() },
            GroupCode { key_id: "0123456789ABCDEF0123456789ABCDEF".into(), ..r2_group() },
            GroupCode { region: "us-east-1".into(), ..r2_group() },
        ] {
            let code = g.encode();
            assert!(code.starts_with("WR1-"), "{code}");
            assert_eq!(GroupCode::decode(&code).unwrap(), g);
        }
        assert!(GroupCode::decode("WR2-AAAA").is_err());
    }

    #[test]
    fn discord_and_plain_invites_yield_the_compact_code() {
        let code = r2_group().encode();
        let msg = discord_invite_message(&code);
        assert!(msg.contains("](<https://"), "{msg}");
        assert_eq!(code_from_invite(&msg).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&invite_link(&code)).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&format!("walkrelay://join#{code}")).as_deref(), Some(code.as_str()));
        // A bogus code earlier in the text doesn't hide a real one later.
        assert_eq!(code_from_invite(&format!("WR2-nope and {code}")).as_deref(), Some(code.as_str()));
    }

    #[test]
    fn invite_links_and_messages_yield_the_code() {
        let g = GroupCode {
            endpoint: "https://abc.r2.cloudflarestorage.com".into(),
            bucket: "big-walk".into(),
            key_id: "KEY".into(),
            secret: "SECRET".into(),
            region: "auto".into(),
        };
        let code = g.encode();
        let link = invite_link(&code);
        assert_eq!(code_from_invite(&code).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&link).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&format!("walkrelay://join#{code}")).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&format!("walkrelay://join/#{code}/")).as_deref(), Some(code.as_str()));
        let msg = format!("Join our Big Walk group on Walk Relay: {link} (keep this private)");
        assert_eq!(code_from_invite(&msg).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite(&format!("<{link}>.")).as_deref(), Some(code.as_str()));
        assert_eq!(code_from_invite("WR1-garbage"), None);
        assert_eq!(code_from_invite("https://example.com/"), None);
        assert_eq!(code_from_invite(""), None);
    }
}
