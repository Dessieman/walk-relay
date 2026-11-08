//! The sync engine.
//!
//! Every cycle it compares, per journey and per local save folder:
//!   * the local files (hashed),
//!   * the remote object (by ETag), and
//!   * what both looked like the last time they were in sync (`JState`).
//!
//! From that it decides to upload, download, delete, or resolve a conflict.
//! Rules that keep saves safe:
//!   * While Big Walk is running nothing in the save folder is ever written;
//!     only uploads of the host's progress happen.
//!   * Uploads are conditional, so a stale copy can never overwrite newer
//!     progress. If two people really did play the same journey separately,
//!     the newer play session wins and the other copy is kept as a restorable
//!     "conflict" version.
//!   * Every local overwrite/delete is backed up first, and every upload is also
//!     kept in remote history.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bundle::{self, Files, Manifest};
use crate::paths::{self, display_name, journey_stem};
use crate::store::{Cond, CountingStore, OpCounter, PutOutcome, RemoteStore};
use crate::now_ms;

const JOURNEYS: &str = "v1/journeys/";
const HISTORY: &str = "v1/history/";
const CONFLICTS: &str = "v1/conflicts/";
const PRESENCE: &str = "v1/presence/";
const TMP_SUFFIX: &str = ".walkrelay-tmp";

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub player: String,
    pub data_dir: PathBuf,
    pub extra_save_dirs: Vec<PathBuf>,
    /// Use exactly these save folders instead of auto-detecting (tests).
    pub save_dirs_override: Option<Vec<PathBuf>>,
    /// A save file must be untouched this long before we read it.
    pub stable_ms: u64,
    /// ... and this long while the game is running (it writes in bursts).
    pub stable_ms_while_running: u64,
    /// A journey must be missing locally this long before its deletion is shared.
    pub delete_confirm_ms: u64,
    pub history_keep: usize,
    pub local_backups_keep: usize,
    pub presence_fresh_ms: u64,
    pub presence_heartbeat_ms: u64,
    /// Sync-state file name inside `data_dir` (one per group).
    pub state_file: String,
    /// Monthly operation-count file name inside `data_dir` (one per group).
    pub usage_file: String,
}

impl EngineOptions {
    pub fn new(player: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            player: player.into(),
            data_dir: data_dir.into(),
            extra_save_dirs: Vec::new(),
            save_dirs_override: None,
            stable_ms: 5_000,
            stable_ms_while_running: 20_000,
            delete_confirm_ms: 90_000,
            history_keep: 20,
            local_backups_keep: 100,
            presence_fresh_ms: 3 * 60_000,
            presence_heartbeat_ms: 60_000,
            state_file: "state.json".into(),
            usage_file: "usage.json".into(),
        }
    }
}

/// Something worth telling the player about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Uploaded { journey: String },
    Downloaded { journey: String, from: String },
    /// Two people played the same journey separately. `kept_from`'s version is
    /// now the current one; `saved_copy_from`'s is kept under "Versions".
    Conflict { journey: String, kept_from: String, saved_copy_from: String },
    DeletedLocal { journey: String },
    DeletedRemote { journey: String },
    PeerStartedHosting { player: String, journey: String },
    /// You launched Big Walk while someone else is already hosting.
    PeerAlreadyHosting { player: String, journey: String },
    /// The group has used most of Cloudflare R2's free monthly allowance.
    /// Sent at most once per month per computer.
    NearFreeLimit { percent: u32 },
}

/// Cloudflare R2 free tier, per month.
pub const FREE_CLASS_A: u64 = 1_000_000;
pub const FREE_CLASS_B: u64 = 10_000_000;
/// Warn once the group passes this share of either allowance.
pub const WARN_AT_PERCENT: u64 = 70;

/// Bucket operations made by one computer in one calendar month (UTC).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub month: String,
    pub class_a: u64,
    pub class_b: u64,
}

/// The whole group's operations this month, as far as this computer knows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupUsage {
    pub month: String,
    pub class_a: u64,
    pub class_b: u64,
    pub free_class_a: u64,
    pub free_class_b: u64,
    /// Highest share of either free allowance used, in percent.
    pub percent: u32,
    pub near_limit: bool,
}

impl GroupUsage {
    pub fn new(month: String, class_a: u64, class_b: u64) -> Self {
        let pa = class_a.saturating_mul(100) / FREE_CLASS_A;
        let pb = class_b.saturating_mul(100) / FREE_CLASS_B;
        let percent = pa.max(pb);
        Self {
            month,
            class_a,
            class_b,
            free_class_a: FREE_CLASS_A,
            free_class_b: FREE_CLASS_B,
            percent: percent.min(u32::MAX as u64) as u32,
            near_limit: percent >= WARN_AT_PERCENT,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageFile {
    #[serde(flatten)]
    usage: Usage,
    #[serde(default)]
    warned_month: Option<String>,
}

/// "YYYY-MM" for a Unix time in milliseconds, in UTC.
pub fn month_utc(ms: u64) -> String {
    // Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub player: String,
    pub game_running: bool,
    /// Journey stem this player is currently hosting.
    pub hosting: Option<String>,
    pub updated_at: u64,
    /// This player's bucket operations this month (missing from older versions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JourneyStatus {
    pub stem: String,
    pub name: String,
    pub last_uploaded_by: Option<String>,
    pub saved_at: Option<u64>,
    pub on_this_computer: bool,
    pub in_sync: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerStatus {
    pub player: String,
    pub game_running: bool,
    pub hosting: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub player: String,
    pub save_dirs: Vec<String>,
    pub game_running: bool,
    pub hosting: Option<String>,
    pub last_sync_at: Option<u64>,
    pub last_error: Option<String>,
    pub journeys: Vec<JourneyStatus>,
    pub peers: Vec<PeerStatus>,
    pub usage: GroupUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    History,
    Conflict,
}

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub key: String,
    pub kind: VersionKind,
    pub player: String,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JState {
    base_etag: String,
    hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// save folder -> journey stem -> last synced state
    roots: BTreeMap<String, BTreeMap<String, JState>>,
}

struct LocalJourney {
    files: Files,
    hash: String,
    newest_mtime: u64,
    has_sav: bool,
}

#[derive(Clone, Copy, Default)]
struct Guards {
    mass_local_delete: bool,
    mass_remote_delete: bool,
}

/// Write a journey's files all-or-nothing: every temp file first, then the
/// renames, then remove files the new version doesn't have. Files get the
/// remote save time as their mtime, so "which session is newer" stays true.
fn write_journey(root: &Path, stem: &str, files: &Files, saved_at: u64) -> Result<()> {
    let mut staged = Vec::new();
    for (name, data) in files {
        let path = root.join(name);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(TMP_SUFFIX);
        let tmp = PathBuf::from(tmp);
        let write = (|| -> Result<()> {
            let mut f = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
            std::io::Write::write_all(&mut f, data)?;
            if saved_at > 0 {
                let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(saved_at));
            }
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            for (t, _) in &staged {
                let _ = std::fs::remove_file(t);
            }
            return Err(e);
        }
        staged.push((tmp, path));
    }
    for (i, (tmp, path)) in staged.iter().enumerate() {
        if let Err(e) = std::fs::rename(tmp, path) {
            for (t, _) in &staged[i..] {
                let _ = std::fs::remove_file(t);
            }
            return Err(e).with_context(|| format!("replacing {} (is Big Walk open?)", path.display()));
        }
    }
    for p in journey_files_on_disk(root, stem) {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if !files.contains_key(&n) {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

struct Remote {
    bytes: Vec<u8>,
    etag: String,
    manifest: Manifest,
    files: Files,
}

pub struct Engine {
    store: Arc<dyn RemoteStore>,
    opts: EngineOptions,
    state: State,
    pending_delete: HashMap<(String, String), u64>,
    manifests: HashMap<String, (String, Manifest)>,
    game_running_prev: bool,
    game_started_at: u64,
    last_presence: Option<(Presence, u64)>,
    peers: BTreeMap<String, Presence>,
    /// Latest reported usage of every other player, fresh or not.
    peer_usage: BTreeMap<String, Usage>,
    counter: Arc<OpCounter>,
    usage: UsageFile,
    status: Status,
}

// ---------------------------------------------------------------- keys

fn hexs(s: &str) -> String {
    hex::encode(s.as_bytes())
}
fn unhexs(s: &str) -> Option<String> {
    String::from_utf8(hex::decode(s).ok()?).ok()
}
fn journey_key(stem: &str) -> String {
    format!("{JOURNEYS}{}.zip", hexs(stem))
}
fn version_key(prefix: &str, stem: &str, at: u64, player: &str) -> String {
    format!("{prefix}{}/{at:013}-{}.zip", hexs(stem), hexs(player))
}
fn presence_key(player: &str) -> String {
    format!("{PRESENCE}{}.json", hexs(player))
}

// ---------------------------------------------------------------- fs helpers

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(TMP_SUFFIX);
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn scan(root: &Path) -> Result<BTreeMap<String, LocalJourney>> {
    let mut out: BTreeMap<String, LocalJourney> = BTreeMap::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Some(stem) = journey_stem(&name) else { continue };
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(_) => continue, // locked by the game; try next cycle
        };
        let j = out.entry(stem.to_string()).or_insert_with(|| LocalJourney {
            files: Files::new(),
            hash: String::new(),
            newest_mtime: 0,
            has_sav: false,
        });
        j.newest_mtime = j.newest_mtime.max(mtime_ms(&meta));
        if name.ends_with(".sav") {
            j.has_sav = true;
        }
        j.files.insert(name, data);
    }
    for j in out.values_mut() {
        j.hash = bundle::content_hash(&j.files);
    }
    Ok(out)
}

fn journey_files_on_disk(root: &Path, stem: &str) -> Vec<PathBuf> {
    [format!("{stem}.sav"), format!("{stem}.sav.bak")]
        .into_iter()
        .map(|n| root.join(n))
        .filter(|p| p.is_file())
        .collect()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------- engine

impl Engine {
    pub fn new(store: Arc<dyn RemoteStore>, opts: EngineOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.data_dir)?;
        let counting = CountingStore::new(store);
        let counter = counting.counter();
        let store: Arc<dyn RemoteStore> = Arc::new(counting);
        let usage = std::fs::read(opts.data_dir.join(&opts.usage_file))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let state = match std::fs::read(opts.data_dir.join(&opts.state_file)) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => State::default(),
        };
        let status = Status { player: opts.player.clone(), ..Default::default() };
        Ok(Self {
            store,
            opts,
            state,
            pending_delete: HashMap::new(),
            manifests: HashMap::new(),
            game_running_prev: false,
            game_started_at: 0,
            last_presence: None,
            peers: BTreeMap::new(),
            peer_usage: BTreeMap::new(),
            counter,
            usage,
            status,
        })
    }

    /// Add the operations counted since last time to this month's total.
    fn fold_usage(&mut self, now: u64) {
        let month = month_utc(now);
        let (a, b) = self.counter.take();
        let u = &mut self.usage.usage;
        if u.month != month {
            *u = Usage { month, class_a: 0, class_b: 0 };
        }
        u.class_a += a;
        u.class_b += b;
    }

    fn group_usage(&self) -> GroupUsage {
        let mine = &self.usage.usage;
        let (mut a, mut b) = (mine.class_a, mine.class_b);
        for u in self.peer_usage.values().filter(|u| u.month == mine.month) {
            a += u.class_a;
            b += u.class_b;
        }
        GroupUsage::new(mine.month.clone(), a, b)
    }

    /// Fold in the latest counts, refresh the status, warn once a month, and save.
    fn update_usage(&mut self, now: u64, events: &mut Vec<Event>) {
        self.fold_usage(now);
        let g = self.group_usage();
        if g.near_limit && self.usage.warned_month.as_deref() != Some(g.month.as_str()) {
            self.usage.warned_month = Some(g.month.clone());
            events.push(Event::NearFreeLimit { percent: g.percent });
        }
        self.status.usage = g;
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.usage) {
            if let Err(e) = write_atomic(&self.opts.data_dir.join(&self.opts.usage_file), &bytes) {
                log::warn!("saving usage: {e:#}");
            }
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn store(&self) -> Arc<dyn RemoteStore> {
        self.store.clone()
    }

    pub fn save_dirs(&self) -> Vec<PathBuf> {
        match &self.opts.save_dirs_override {
            Some(v) => v.iter().filter(|p| p.is_dir()).cloned().collect(),
            None => paths::existing_save_dirs(&self.opts.extra_save_dirs),
        }
    }

    fn save_state(&self) -> Result<()> {
        write_atomic(&self.opts.data_dir.join(&self.opts.state_file), &serde_json::to_vec_pretty(&self.state)?)
    }

    /// One full sync pass. Call every ~30 s, and right after Big Walk exits.
    pub fn sync_once(&mut self, game_running: bool) -> Result<Vec<Event>> {
        let result = self.sync_inner(game_running);
        match &result {
            Ok(_) => {
                self.status.last_sync_at = Some(now_ms());
                self.status.last_error = self.status.last_error.take().filter(|e| e.starts_with("journey "));
            }
            Err(e) => self.status.last_error = Some(format!("{e:#}")),
        }
        result
    }

    fn sync_inner(&mut self, game_running: bool) -> Result<Vec<Event>> {
        let now = now_ms();
        let mut events = Vec::new();
        self.status.last_error = None;

        let just_launched = game_running && !self.game_running_prev;
        if just_launched {
            self.game_started_at = now;
        }
        self.game_running_prev = game_running;

        // Remote journeys: stem -> etag
        let mut remote: BTreeMap<String, String> = BTreeMap::new();
        for o in self.store.list(JOURNEYS)? {
            let Some(hex_stem) = o.key.strip_prefix(JOURNEYS).and_then(|k| k.strip_suffix(".zip")) else { continue };
            if let Some(stem) = unhexs(hex_stem) {
                remote.insert(stem, o.etag);
            }
        }

        let roots = self.save_dirs();
        let mut scans = Vec::new();
        for root in &roots {
            scans.push(scan(root)?);
        }
        let present_anywhere: BTreeSet<String> = scans.iter().flat_map(|m| m.keys().cloned()).collect();
        let guards = self.deletion_guards(&roots, &scans, &remote, &present_anywhere);

        let mut hosting: Option<(u64, String)> = None;
        let mut local_union: BTreeMap<String, bool> = BTreeMap::new(); // stem -> in sync everywhere
        for (root, local) in roots.iter().zip(scans) {
            let local = self.sync_root(root, local, &present_anywhere, guards, &mut remote, game_running, &mut events)?;
            for (stem, mtime, in_sync) in local {
                let e = local_union.entry(stem.clone()).or_insert(true);
                *e &= in_sync;
                if game_running && mtime > self.game_started_at && hosting.as_ref().map_or(true, |(m, _)| mtime > *m) {
                    hosting = Some((mtime, stem));
                }
            }
        }
        let hosting = hosting.map(|(_, s)| s);

        // Presence: publish ours, read everyone else's.
        self.fold_usage(now);
        let mine = Presence {
            player: self.opts.player.clone(),
            game_running,
            hosting: hosting.clone(),
            updated_at: now,
            usage: Some(self.usage.usage.clone()),
        };
        let changed = self.last_presence.as_ref().map_or(true, |(p, at)| {
            p.game_running != mine.game_running || p.hosting != mine.hosting || now.saturating_sub(*at) >= self.opts.presence_heartbeat_ms
        });
        if changed {
            self.store.put(&presence_key(&self.opts.player), &serde_json::to_vec(&mine)?, Cond::None)?;
            self.last_presence = Some((mine, now));
        }
        self.read_peers(now, &mut events)?;
        if just_launched {
            // Warn from this pass's presence, not the previous pass's (which can be
            // minutes old when syncing at the idle pace).
            for p in self.peers.values() {
                if let Some(h) = &p.hosting {
                    let journey = display_name(h);
                    events.retain(|e| !matches!(e, Event::PeerStartedHosting { player, .. } if *player == p.player));
                    events.push(Event::PeerAlreadyHosting { player: p.player.clone(), journey });
                }
            }
        }

        // Status for the UI.
        let mut journeys = Vec::new();
        let all: BTreeSet<String> = remote.keys().chain(local_union.keys()).cloned().collect();
        for stem in all {
            let manifest = match remote.get(&stem) {
                Some(etag) => self.manifest_for(&stem, etag).ok(),
                None => None,
            };
            journeys.push(JourneyStatus {
                name: display_name(&stem),
                last_uploaded_by: manifest.as_ref().map(|m| m.uploaded_by.clone()),
                saved_at: manifest.as_ref().map(|m| m.saved_at),
                on_this_computer: local_union.contains_key(&stem),
                in_sync: remote.contains_key(&stem) && local_union.get(&stem).copied().unwrap_or(false),
                stem,
            });
        }
        journeys.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then(a.name.cmp(&b.name)));
        self.status.journeys = journeys;
        self.status.save_dirs = roots.iter().map(|p| p.display().to_string()).collect();
        self.status.game_running = game_running;
        self.status.hosting = hosting.as_deref().map(display_name);
        self.status.peers = self
            .peers
            .values()
            .map(|p| PeerStatus { player: p.player.clone(), game_running: p.game_running, hosting: p.hosting.as_deref().map(display_name) })
            .collect();

        self.update_usage(now, &mut events);
        self.save_state()?;
        self.prune_local_backups();
        Ok(events)
    }

    fn read_peers(&mut self, now: u64, events: &mut Vec<Event>) -> Result<()> {
        let mut fresh = BTreeMap::new();
        let mut usage = BTreeMap::new();
        for o in self.store.list(PRESENCE)? {
            if o.key == presence_key(&self.opts.player) {
                continue;
            }
            let Some((bytes, _)) = self.store.get(&o.key)? else { continue };
            let Ok(p) = serde_json::from_slice::<Presence>(&bytes) else { continue };
            if let Some(u) = &p.usage {
                usage.insert(p.player.clone(), u.clone());
            }
            if now.saturating_sub(p.updated_at) <= self.opts.presence_fresh_ms && (p.game_running || p.hosting.is_some()) {
                fresh.insert(p.player.clone(), p);
            }
        }
        for p in fresh.values() {
            if let Some(h) = &p.hosting {
                let before = self.peers.get(&p.player).and_then(|q| q.hosting.as_ref());
                if before != Some(h) {
                    events.push(Event::PeerStartedHosting { player: p.player.clone(), journey: display_name(h) });
                }
            }
        }
        self.peers = fresh;
        self.peer_usage = usage;
        Ok(())
    }

    /// Tell everyone we're offline (call on quit).
    pub fn clear_presence(&mut self) -> Result<()> {
        self.fold_usage(now_ms());
        let p = Presence {
            player: self.opts.player.clone(),
            game_running: false,
            hosting: None,
            updated_at: now_ms(),
            usage: Some(self.usage.usage.clone()),
        };
        self.store.put(&presence_key(&self.opts.player), &serde_json::to_vec(&p)?, Cond::None)?;
        self.last_presence = Some((p, now_ms()));
        Ok(())
    }

    /// Deletions are only shared one journey at a time. If several journeys
    /// vanish at once (a folder emptied, a reinstall, a wrong bucket), that's
    /// treated as an accident: everything is restored instead of deleted.
    fn deletion_guards(
        &self,
        roots: &[PathBuf],
        scans: &[BTreeMap<String, LocalJourney>],
        remote: &BTreeMap<String, String>,
        present_anywhere: &BTreeSet<String>,
    ) -> Guards {
        let mut local_gone = BTreeSet::new();
        let mut remote_gone = BTreeSet::new();
        for (root, local) in roots.iter().zip(scans) {
            let Some(st) = self.state.roots.get(&root.display().to_string()) else { continue };
            for (stem, s) in st {
                match (local.get(stem), remote.get(stem)) {
                    (None, Some(r)) if *r == s.base_etag && !present_anywhere.contains(stem) => {
                        local_gone.insert(stem.clone());
                    }
                    (Some(l), None) if l.hash == s.hash => {
                        remote_gone.insert(stem.clone());
                    }
                    _ => {}
                }
            }
        }
        Guards { mass_local_delete: local_gone.len() > 1, mass_remote_delete: remote_gone.len() > 1 }
    }

    /// Returns (stem, newest mtime, in sync) for each local journey.
    #[allow(clippy::too_many_arguments)]
    fn sync_root(
        &mut self,
        root: &Path,
        local: BTreeMap<String, LocalJourney>,
        present_anywhere: &BTreeSet<String>,
        guards: Guards,
        remote: &mut BTreeMap<String, String>,
        game_running: bool,
        events: &mut Vec<Event>,
    ) -> Result<Vec<(String, u64, bool)>> {
        let now = now_ms();
        let root_key = root.display().to_string();
        let stable = if game_running { self.opts.stable_ms_while_running } else { self.opts.stable_ms };
        let known: Vec<String> = self.state.roots.get(&root_key).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let stems: BTreeSet<String> = local.keys().chain(remote.keys()).chain(known.iter()).cloned().collect();

        for stem in stems {
            let l = local.get(&stem);
            let r = remote.get(&stem).cloned();
            let s = self.state.roots.get(&root_key).and_then(|m| m.get(&stem)).cloned();
            if let Some(l) = l {
                self.pending_delete.remove(&(root_key.clone(), stem.clone()));
                if !l.has_sav || now.saturating_sub(l.newest_mtime) < stable {
                    continue; // mid-write; look again next cycle
                }
            }
            let keep = guards.mass_local_delete || present_anywhere.contains(&stem);
            let res = self.sync_journey(root, &root_key, &stem, l, r, s, game_running, keep, guards.mass_remote_delete, remote, events);
            if let Err(e) = res {
                log::warn!("{stem}: {e:#}");
                self.status.last_error = Some(format!("journey {}: {e:#}", display_name(&stem)));
            }
        }

        // Report what's on disk now (downloads/deletes above changed it).
        let local = scan(root)?;
        let st = self.state.roots.get(&root_key);
        Ok(local
            .iter()
            .filter(|(_, l)| l.has_sav)
            .map(|(stem, l)| {
                let in_sync = st
                    .and_then(|m| m.get(stem))
                    .map_or(false, |s| s.hash == l.hash && remote.get(stem) == Some(&s.base_etag));
                (stem.clone(), l.newest_mtime, in_sync)
            })
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn sync_journey(
        &mut self,
        root: &Path,
        root_key: &str,
        stem: &str,
        l: Option<&LocalJourney>,
        r: Option<String>,
        s: Option<JState>,
        game_running: bool,
        // Don't share a local deletion (journey still elsewhere on this machine, or many vanished at once).
        keep_local_missing: bool,
        // Don't apply a remote deletion (many vanished remotely at once).
        keep_remote_missing: bool,
        remote: &mut BTreeMap<String, String>,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        let name = display_name(stem);
        match (l, r, s) {
            (None, None, Some(_)) => {
                self.set_state(root_key, stem, None);
            }
            (None, None, None) => {}

            // New journey here that nobody has yet.
            (Some(l), None, None) => {
                self.upload(root_key, stem, l, Cond::IfNoneMatch, remote, events)?;
            }
            // We had it in sync, and it vanished remotely: someone deleted it.
            (Some(l), None, Some(s)) => {
                if l.hash == s.hash && !keep_remote_missing {
                    if !game_running {
                        self.backup_local(root, stem)?;
                        for p in journey_files_on_disk(root, stem) {
                            std::fs::remove_file(p)?;
                        }
                        self.set_state(root_key, stem, None);
                        events.push(Event::DeletedLocal { journey: name });
                    }
                } else {
                    // We kept playing it; bring it back with our progress.
                    self.upload(root_key, stem, l, Cond::IfNoneMatch, remote, events)?;
                }
            }

            // Someone else's journey we don't have yet.
            (None, Some(_), None) => {
                if !game_running {
                    self.download(root, root_key, stem, None, events)?;
                }
            }
            // We had it, now it's gone locally.
            (None, Some(r), Some(s)) => {
                if r == s.base_etag && keep_local_missing {
                    if !game_running {
                        self.download(root, root_key, stem, None, events)?;
                    }
                } else if r == s.base_etag {
                    let k = (root_key.to_string(), stem.to_string());
                    let first = *self.pending_delete.entry(k.clone()).or_insert_with(now_ms);
                    if !game_running && now_ms().saturating_sub(first) >= self.opts.delete_confirm_ms {
                        self.store.delete(&journey_key(stem))?;
                        remote.remove(stem);
                        self.pending_delete.remove(&k);
                        self.set_state(root_key, stem, None);
                        events.push(Event::DeletedRemote { journey: name });
                    }
                } else if !game_running {
                    // Others progressed it after we deleted it; their progress wins.
                    self.download(root, root_key, stem, None, events)?;
                }
            }

            (Some(l), Some(r), s) => {
                let local_changed = s.as_ref().map_or(true, |s| s.hash != l.hash);
                let remote_changed = s.as_ref().map_or(true, |s| s.base_etag != r);
                match (local_changed, remote_changed) {
                    (false, false) => {}
                    (true, false) => {
                        self.upload(root_key, stem, l, Cond::IfMatch(r), remote, events)?;
                    }
                    (false, true) => {
                        if !game_running {
                            self.download(root, root_key, stem, Some(l), events)?;
                        }
                    }
                    (true, true) => {
                        let rem = self.fetch(stem)?;
                        match rem {
                            None => {}
                            Some(rem) if rem.manifest.content_hash == l.hash => {
                                self.set_state(root_key, stem, Some(JState { base_etag: rem.etag, hash: l.hash.clone() }));
                            }
                            Some(rem) => {
                                if !game_running {
                                    self.resolve_conflict(root, root_key, stem, l, rem, remote, events)?;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_conflict(
        &mut self,
        root: &Path,
        root_key: &str,
        stem: &str,
        l: &LocalJourney,
        rem: Remote,
        remote: &mut BTreeMap<String, String>,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        let name = display_name(stem);
        let now = now_ms();
        let me = self.opts.player.clone();
        if l.newest_mtime > rem.manifest.saved_at {
            // Our session is newer: keep theirs as a conflict copy, publish ours.
            self.store.put(&version_key(CONFLICTS, stem, now, &rem.manifest.uploaded_by), &rem.bytes, Cond::None)?;
            if self.upload(root_key, stem, l, Cond::IfMatch(rem.etag.clone()), remote, events)? {
                events.retain(|e| !matches!(e, Event::Uploaded { journey } if *journey == name));
                events.push(Event::Conflict { journey: name, kept_from: me, saved_copy_from: rem.manifest.uploaded_by });
            }
        } else {
            // Theirs is newer: keep ours as a conflict copy, take theirs.
            let ours = self.pack_local(stem, l)?;
            self.store.put(&version_key(CONFLICTS, stem, now, &me), &ours, Cond::None)?;
            let from = rem.manifest.uploaded_by.clone();
            self.write_remote(root, root_key, stem, rem)?;
            events.push(Event::Conflict { journey: name, kept_from: from, saved_copy_from: me });
        }
        Ok(())
    }

    fn pack_local(&self, stem: &str, l: &LocalJourney) -> Result<Vec<u8>> {
        let manifest = Manifest {
            format: 1,
            journey: stem.to_string(),
            uploaded_by: self.opts.player.clone(),
            uploaded_at: now_ms(),
            saved_at: l.newest_mtime,
            content_hash: l.hash.clone(),
        };
        bundle::pack(&manifest, &l.files)
    }

    /// Returns true if the upload went through.
    fn upload(
        &mut self,
        root_key: &str,
        stem: &str,
        l: &LocalJourney,
        cond: Cond,
        remote: &mut BTreeMap<String, String>,
        events: &mut Vec<Event>,
    ) -> Result<bool> {
        let bytes = self.pack_local(stem, l)?;
        match self.store.put(&journey_key(stem), &bytes, cond)? {
            PutOutcome::Ok { etag } => {
                self.set_state(root_key, stem, Some(JState { base_etag: etag.clone(), hash: l.hash.clone() }));
                remote.insert(stem.to_string(), etag.clone());
                if let Ok((m, _)) = bundle::unpack(&bytes) {
                    self.manifests.insert(stem.to_string(), (etag, m));
                }
                let hk = version_key(HISTORY, stem, now_ms(), &self.opts.player);
                if let Err(e) = self.store.put(&hk, &bytes, Cond::None).and_then(|_| self.prune_history(stem)) {
                    log::warn!("history for {stem}: {e:#}");
                }
                events.push(Event::Uploaded { journey: display_name(stem) });
                Ok(true)
            }
            PutOutcome::PreconditionFailed => {
                log::info!("{stem}: remote changed while uploading; will reconcile next cycle");
                Ok(false)
            }
        }
    }

    fn fetch(&mut self, stem: &str) -> Result<Option<Remote>> {
        let Some((bytes, etag)) = self.store.get(&journey_key(stem))? else { return Ok(None) };
        let (manifest, files) = bundle::unpack(&bytes)?;
        self.manifests.insert(stem.to_string(), (etag.clone(), manifest.clone()));
        Ok(Some(Remote { bytes, etag, manifest, files }))
    }

    fn manifest_for(&mut self, stem: &str, etag: &str) -> Result<Manifest> {
        if let Some((e, m)) = self.manifests.get(stem) {
            if e == etag {
                return Ok(m.clone());
            }
        }
        let r = self.fetch(stem)?.context("gone")?;
        Ok(r.manifest)
    }

    fn download(
        &mut self,
        root: &Path,
        root_key: &str,
        stem: &str,
        local: Option<&LocalJourney>,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        let Some(rem) = self.fetch(stem)? else { return Ok(()) };
        if local.map_or(false, |l| l.hash == rem.manifest.content_hash) {
            self.set_state(root_key, stem, Some(JState { base_etag: rem.etag, hash: rem.manifest.content_hash }));
            return Ok(());
        }
        let from = rem.manifest.uploaded_by.clone();
        self.write_remote(root, root_key, stem, rem)?;
        events.push(Event::Downloaded { journey: display_name(stem), from });
        Ok(())
    }

    fn write_remote(&mut self, root: &Path, root_key: &str, stem: &str, rem: Remote) -> Result<()> {
        let backup = self.backup_local(root, stem)?;
        if let Err(e) = write_journey(root, stem, &rem.files, rem.manifest.saved_at) {
            // Put the previous pair back so the folder never holds a mixed .sav/.bak.
            if let Some(dir) = backup {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for f in rd.flatten() {
                        let _ = std::fs::copy(f.path(), root.join(f.file_name()));
                    }
                }
            }
            return Err(e);
        }
        self.set_state(root_key, stem, Some(JState { base_etag: rem.etag, hash: rem.manifest.content_hash }));
        Ok(())
    }

    fn set_state(&mut self, root_key: &str, stem: &str, s: Option<JState>) {
        let m = self.state.roots.entry(root_key.to_string()).or_default();
        match s {
            Some(s) => {
                m.insert(stem.to_string(), s);
            }
            None => {
                m.remove(stem);
            }
        }
        if let Err(e) = self.save_state() {
            log::warn!("saving sync state: {e:#}");
        }
    }

    fn backup_local(&self, root: &Path, stem: &str) -> Result<Option<PathBuf>> {
        let files = journey_files_on_disk(root, stem);
        if files.is_empty() {
            return Ok(None);
        }
        let dir = self
            .opts
            .data_dir
            .join("backups")
            .join(format!("{:013}-{}", now_ms(), sanitize(stem)));
        std::fs::create_dir_all(&dir)?;
        for f in files {
            if let Some(n) = f.file_name() {
                std::fs::copy(&f, dir.join(n)).with_context(|| format!("backing up {}", f.display()))?;
            }
        }
        Ok(Some(dir))
    }

    fn prune_local_backups(&self) {
        let dir = self.opts.data_dir.join("backups");
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        names.sort();
        let excess = names.len().saturating_sub(self.opts.local_backups_keep);
        for p in names.into_iter().take(excess) {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    fn prune_history(&self, stem: &str) -> Result<()> {
        let mut keys: Vec<String> = self
            .store
            .list(&format!("{HISTORY}{}/", hexs(stem)))?
            .into_iter()
            .map(|o| o.key)
            .collect();
        keys.sort();
        let excess = keys.len().saturating_sub(self.opts.history_keep);
        for k in keys.into_iter().take(excess) {
            self.store.delete(&k)?;
        }
        Ok(())
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.opts.data_dir.join("backups")
    }
}

// ---------------------------------------------------------------- versions

/// Earlier versions of a journey (newest first): every upload, plus any
/// copies set aside because of a conflict.
pub fn list_versions(store: &dyn RemoteStore, stem: &str) -> Result<Vec<Version>> {
    let mut out = Vec::new();
    for (prefix, kind) in [(HISTORY, VersionKind::History), (CONFLICTS, VersionKind::Conflict)] {
        let base = format!("{prefix}{}/", hexs(stem));
        for o in store.list(&base)? {
            let Some(rest) = o.key.strip_prefix(&base).and_then(|k| k.strip_suffix(".zip")) else { continue };
            let Some((at, player)) = rest.split_once('-') else { continue };
            out.push(Version {
                kind,
                at: at.parse().unwrap_or(0),
                player: unhexs(player).unwrap_or_default(),
                key: o.key,
            });
        }
    }
    out.sort_by(|a, b| b.at.cmp(&a.at));
    Ok(out)
}

/// Journeys that were deleted but still have saved versions (stem, name), so
/// they can be restored.
pub fn list_deleted(store: &dyn RemoteStore) -> Result<Vec<(String, String)>> {
    let current: BTreeSet<String> = store
        .list(JOURNEYS)?
        .into_iter()
        .filter_map(|o| o.key.strip_prefix(JOURNEYS)?.strip_suffix(".zip").and_then(unhexs))
        .collect();
    let mut out = BTreeSet::new();
    for prefix in [HISTORY, CONFLICTS] {
        for o in store.list(prefix)? {
            if let Some(stem) = o.key.strip_prefix(prefix).and_then(|k| k.split('/').next()).and_then(unhexs) {
                if !current.contains(&stem) {
                    out.insert(stem);
                }
            }
        }
    }
    Ok(out.into_iter().map(|s| { let n = display_name(&s); (s, n) }).collect())
}

/// Make an earlier version the current one. Everyone picks it up on their
/// next sync (once their Big Walk is closed).
pub fn restore_version(store: &dyn RemoteStore, stem: &str, key: &str, player: &str) -> Result<()> {
    anyhow::ensure!(
        key.starts_with(&format!("{HISTORY}{}/", hexs(stem))) || key.starts_with(&format!("{CONFLICTS}{}/", hexs(stem))),
        "that version belongs to a different journey"
    );
    let (bytes, _) = store.get(key)?.context("that version no longer exists")?;
    let (mut manifest, files) = bundle::unpack(&bytes)?;
    let now = now_ms();
    manifest.uploaded_by = format!("{player} (restored)");
    manifest.uploaded_at = now;
    manifest.saved_at = now;
    let packed = bundle::pack(&manifest, &files)?;
    for _ in 0..5 {
        let current = store.list(&journey_key(stem))?.into_iter().find(|o| o.key == journey_key(stem));
        let cond = match current {
            Some(o) => Cond::IfMatch(o.etag),
            None => Cond::IfNoneMatch,
        };
        if let PutOutcome::Ok { .. } = store.put(&journey_key(stem), &packed, cond)? {
            store.put(&version_key(HISTORY, stem, now, player), &packed, Cond::None)?;
            return Ok(());
        }
    }
    anyhow::bail!("someone else is uploading this journey right now; try again in a minute")
}
