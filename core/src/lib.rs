//! Walk Relay core: keeps every friend's Big Walk `save_games` folder identical,
//! so whoever is around can host the latest journey.
//!
//! The remote side is any S3-compatible bucket (Cloudflare R2 in practice) that
//! supports conditional PUTs. Each journey (`<stem>.sav` + `<stem>.sav.bak`) is
//! stored as one zip object, so a journey always travels as a consistent pair.

pub mod bundle;
pub mod config;
pub mod engine;
pub mod game;
pub mod paths;
pub mod store;

pub use config::{AppConfig, GroupCode};
pub use engine::{Engine, EngineOptions, Event, Presence, Status, Version, VersionKind};
pub use game::GameDetector;
pub use store::{Cond, MemoryStore, ObjInfo, PutOutcome, R2Store, RemoteStore};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
