//! End-to-end behaviour of the sync engine between several simulated friends
//! sharing one in-memory bucket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use walkrelay_core::engine::{list_versions, restore_version};
use walkrelay_core::{Engine, EngineOptions, Event, MemoryStore, VersionKind};

const STEM: &str = "save_a nice view_52057433e2c14fbb";

struct Friend {
    _tmp: TempDir,
    saves: PathBuf,
    engine: Engine,
}

impl Friend {
    fn new(name: &str, store: &MemoryStore) -> Self {
        let tmp = TempDir::new().unwrap();
        let saves = tmp.path().join("save_games");
        std::fs::create_dir_all(&saves).unwrap();
        let mut opts = EngineOptions::new(name, tmp.path().join("data"));
        opts.save_dirs_override = Some(vec![saves.clone()]);
        opts.stable_ms = 0;
        opts.stable_ms_while_running = 0;
        opts.delete_confirm_ms = 0;
        let engine = Engine::new(Arc::new(store.clone()), opts).unwrap();
        Self { _tmp: tmp, saves, engine }
    }

    fn play(&self, stem: &str, contents: &str) {
        write(&self.saves, &format!("{stem}.sav"), contents, None);
        write(&self.saves, &format!("{stem}.sav.bak"), &format!("{contents}-bak"), None);
    }

    fn play_at(&self, stem: &str, contents: &str, secs_ago: u64) {
        write(&self.saves, &format!("{stem}.sav"), contents, Some(secs_ago));
        write(&self.saves, &format!("{stem}.sav.bak"), &format!("{contents}-bak"), Some(secs_ago));
    }

    fn read(&self, stem: &str) -> Option<String> {
        std::fs::read_to_string(self.saves.join(format!("{stem}.sav"))).ok()
    }

    fn sync(&mut self) -> Vec<Event> {
        self.engine.sync_once(false).unwrap()
    }

    fn sync_running(&mut self) -> Vec<Event> {
        self.engine.sync_once(true).unwrap()
    }
}

fn write(dir: &Path, name: &str, contents: &str, secs_ago: Option<u64>) {
    let p = dir.join(name);
    std::fs::write(&p, contents).unwrap();
    if let Some(s) = secs_ago {
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(s)).unwrap();
    }
}

#[test]
fn host_progress_reaches_everyone_and_anyone_can_host_next() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    let mut sam = Friend::new("Sam", &store);

    chris.play(STEM, "day1");
    assert!(chris.sync().contains(&Event::Uploaded { journey: "a nice view".into() }));

    let ev = alex.sync();
    assert!(ev.contains(&Event::Downloaded { journey: "a nice view".into(), from: "Chris".into() }), "{ev:?}");
    sam.sync();
    assert_eq!(alex.read(STEM).as_deref(), Some("day1"));
    assert_eq!(sam.read(STEM).as_deref(), Some("day1"));
    assert_eq!(
        std::fs::read_to_string(sam.saves.join(format!("{STEM}.sav.bak"))).unwrap(),
        "day1-bak"
    );

    // Chris is away; Alex hosts the next leg.
    alex.play(STEM, "day2");
    alex.sync();
    chris.sync();
    sam.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("day2"));
    assert_eq!(sam.read(STEM).as_deref(), Some("day2"));

    // Everyone is now quietly in sync: no further events.
    assert!(chris.sync().is_empty());
    assert!(alex.sync().is_empty());
    assert!(chris.engine.status().journeys.iter().all(|j| j.in_sync));
}

#[test]
fn nothing_is_written_while_big_walk_is_running_but_host_progress_uploads() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    // Alex is in a session (as a guest); Chris keeps hosting.
    alex.sync_running();
    chris.sync_running();
    chris.play(STEM, "v2");
    chris.sync_running();
    assert!(alex.sync_running().iter().all(|e| !matches!(e, Event::Downloaded { .. })));
    assert_eq!(alex.read(STEM).as_deref(), Some("v1"), "must not touch saves while game runs");

    // Alex quits Big Walk: now it updates.
    alex.sync();
    assert_eq!(alex.read(STEM).as_deref(), Some("v2"));
}

#[test]
fn stale_copy_never_overwrites_newer_progress() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    // Alex hosts and uploads v2. Chris's machine is off.
    alex.play(STEM, "v2");
    alex.sync();

    // Chris comes back: has v1 untouched locally -> must download v2, not upload v1.
    chris.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("v2"));
}

#[test]
fn two_people_hosting_the_same_journey_keeps_both_versions() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    // Both play offline: Alex played earlier, Chris more recently.
    alex.play_at(STEM, "alex-session", 3600);
    chris.play_at(STEM, "chris-session", 60);
    alex.sync(); // Alex uploads first
    let ev = chris.sync(); // Chris's newer session wins, Alex's is set aside
    assert!(
        ev.iter().any(|e| matches!(e, Event::Conflict { kept_from, saved_copy_from, .. } if kept_from == "Chris" && saved_copy_from == "Alex")),
        "{ev:?}"
    );
    alex.sync();
    assert_eq!(alex.read(STEM).as_deref(), Some("chris-session"));

    let versions = list_versions(&store, STEM).unwrap();
    let conflict = versions.iter().find(|v| v.kind == VersionKind::Conflict).expect("conflict copy kept");
    assert_eq!(conflict.player, "Alex");

    // Alex's version can be restored for everyone.
    restore_version(&store, STEM, &conflict.key, "Alex").unwrap();
    chris.sync();
    alex.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("alex-session"));
    assert_eq!(alex.read(STEM).as_deref(), Some("alex-session"));
}

#[test]
fn older_offline_session_loses_but_is_kept() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    chris.play_at(STEM, "chris-old", 7200);
    alex.play_at(STEM, "alex-new", 30);
    alex.sync();
    let ev = chris.sync();
    assert!(ev.iter().any(|e| matches!(e, Event::Conflict { kept_from, .. } if kept_from == "Alex")), "{ev:?}");
    assert_eq!(chris.read(STEM).as_deref(), Some("alex-new"));
    assert!(list_versions(&store, STEM).unwrap().iter().any(|v| v.kind == VersionKind::Conflict && v.player == "Chris"));
}

#[test]
fn first_install_with_identical_saves_just_adopts() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    chris.play(STEM, "same");
    chris.sync();

    // Alex already had a manually-copied identical save before installing.
    let mut alex = Friend::new("Alex", &store);
    alex.play(STEM, "same");
    let ev = alex.sync();
    assert!(ev.is_empty(), "{ev:?}");
    assert!(alex.engine.status().journeys[0].in_sync);
}

#[test]
fn new_journeys_from_several_people_all_spread() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    let other = "save_ready walker one_401fe41c4c9041c6";
    chris.play(STEM, "c");
    alex.play(other, "a");
    chris.sync();
    alex.sync();
    chris.sync();
    assert_eq!(chris.read(other).as_deref(), Some("a"));
    assert_eq!(alex.read(STEM).as_deref(), Some("c"));
}

#[test]
fn deleting_a_journey_in_game_deletes_it_for_everyone_with_backups() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    for ext in [".sav", ".sav.bak"] {
        std::fs::remove_file(chris.saves.join(format!("{STEM}{ext}"))).unwrap();
    }
    chris.sync(); // first sighting starts the confirm timer (0 ms in tests)
    let ev = chris.sync();
    assert!(ev.contains(&Event::DeletedRemote { journey: "a nice view".into() }) || store.keys().iter().all(|k| !k.starts_with("v1/journeys/")), "{ev:?}");
    let ev = alex.sync();
    assert!(ev.contains(&Event::DeletedLocal { journey: "a nice view".into() }), "{ev:?}");
    assert!(alex.read(STEM).is_none());
    // The old version is still restorable from history.
    assert!(!list_versions(&store, STEM).unwrap().is_empty());
}

#[test]
fn file_that_only_has_a_backup_is_left_alone() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    write(&chris.saves, &format!("{STEM}.sav.bak"), "bak-only", None);
    assert!(chris.sync().is_empty());
    assert!(store.keys().iter().all(|k| !k.starts_with("v1/journeys/")));
}

#[test]
fn presence_shows_who_is_hosting() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();

    chris.sync_running(); // Chris launches Big Walk
    std::thread::sleep(Duration::from_millis(20));
    chris.play(STEM, "v2"); // ...and the game saves: Chris is hosting
    chris.sync_running();
    assert_eq!(chris.engine.status().hosting.as_deref(), Some("a nice view"));

    let ev = alex.sync();
    assert!(ev.contains(&Event::PeerStartedHosting { player: "Chris".into(), journey: "a nice view".into() }), "{ev:?}");
    // Alex launches the game while Chris is hosting -> warned.
    let ev = alex.sync_running();
    assert!(ev.contains(&Event::PeerAlreadyHosting { player: "Chris".into(), journey: "a nice view".into() }), "{ev:?}");
}

#[test]
fn history_is_pruned() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    for i in 0..25 {
        chris.play(STEM, &format!("v{i}"));
        chris.sync();
        std::thread::sleep(Duration::from_millis(2));
    }
    let n = list_versions(&store, STEM).unwrap().len();
    assert_eq!(n, 20);
}

#[test]
fn unrelated_files_are_never_touched() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    write(&chris.saves, "settings.json", "{}", None);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();
    assert!(!alex.saves.join("settings.json").exists());
    assert!(store.keys().iter().all(|k| !k.contains("settings")));
}

#[test]
fn status_is_accurate_right_after_a_download() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();
    let j = &alex.engine.status().journeys[0];
    assert!(j.on_this_computer && j.in_sync, "{j:?}");
}

#[test]
fn emptying_a_save_folder_restores_instead_of_deleting_for_everyone() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    let other = "save_ready walker one_401fe41c4c9041c6";
    chris.play(STEM, "a");
    chris.play(other, "b");
    chris.sync();
    alex.sync();
    // Chris's folder gets wiped (reinstall, cleanup...).
    for e in std::fs::read_dir(&chris.saves).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    chris.sync();
    chris.sync();
    alex.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("a"), "restored, not deleted");
    assert_eq!(chris.read(other).as_deref(), Some("b"));
    assert_eq!(alex.read(STEM).as_deref(), Some("a"));
    assert_eq!(alex.read(other).as_deref(), Some("b"));
}

#[test]
fn two_save_folders_on_one_mac_deleting_duplicates_does_not_delete_remotely() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    chris.play(STEM, "a");
    chris.sync();

    let tmp = TempDir::new().unwrap();
    let real = tmp.path().join("House House");
    let unity = tmp.path().join("unity");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&unity).unwrap();
    let mut opts = EngineOptions::new("MacFriend", tmp.path().join("data"));
    opts.save_dirs_override = Some(vec![real.clone(), unity.clone()]);
    opts.stable_ms = 0;
    opts.delete_confirm_ms = 0;
    let mut mac = Engine::new(Arc::new(store.clone()), opts).unwrap();
    mac.sync_once(false).unwrap();
    assert!(real.join(format!("{STEM}.sav")).exists() && unity.join(format!("{STEM}.sav")).exists());

    // The duplicate in the unused folder gets deleted.
    for ext in [".sav", ".sav.bak"] {
        std::fs::remove_file(unity.join(format!("{STEM}{ext}"))).unwrap();
    }
    mac.sync_once(false).unwrap();
    mac.sync_once(false).unwrap();
    chris.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("a"), "journey must survive for everyone");
    assert!(store.keys().iter().any(|k| k.starts_with("v1/journeys/")));
}

#[test]
fn downloaded_files_keep_the_original_save_time() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play_at(STEM, "v1", 3600);
    chris.sync();
    alex.sync();
    let m = std::fs::metadata(alex.saves.join(format!("{STEM}.sav"))).unwrap().modified().unwrap();
    let age = SystemTime::now().duration_since(m).unwrap().as_secs();
    assert!((3500..3700).contains(&age), "mtime should be the save time, was {age}s ago");
}

#[test]
fn walk_names_with_dots_sync() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    let stem = "save_wait.. what_0123456789abcdef";
    chris.play(stem, "dots");
    chris.sync();
    alex.sync();
    assert_eq!(alex.read(stem).as_deref(), Some("dots"));
}

#[test]
fn deleted_journeys_can_be_found_and_restored() {
    use walkrelay_core::engine::list_deleted;
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    chris.play(STEM, "v1");
    chris.sync();
    for ext in [".sav", ".sav.bak"] {
        std::fs::remove_file(chris.saves.join(format!("{STEM}{ext}"))).unwrap();
    }
    chris.sync();
    chris.sync();
    let deleted = list_deleted(&store).unwrap();
    assert_eq!(deleted, vec![(STEM.to_string(), "a nice view".to_string())]);
    let v = &list_versions(&store, STEM).unwrap()[0];
    restore_version(&store, STEM, &v.key, "Chris").unwrap();
    chris.sync();
    assert_eq!(chris.read(STEM).as_deref(), Some("v1"));
    assert!(list_deleted(&store).unwrap().is_empty());
}

#[test]
fn backup_only_journey_does_not_count_as_out_of_sync() {
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    chris.play(STEM, "v1");
    write(&chris.saves, "save_orphan_0123456789abcdef.sav.bak", "x", None);
    chris.sync();
    assert!(chris.engine.status().journeys.iter().all(|j| j.in_sync), "{:?}", chris.engine.status().journeys);
}

#[test]
fn usage_is_counted_shared_and_summed_for_the_group() {
    use walkrelay_core::engine::month_utc;
    let store = MemoryStore::new();
    let mut chris = Friend::new("Chris", &store);
    let mut alex = Friend::new("Alex", &store);
    chris.play(STEM, "v1");
    chris.sync();
    alex.sync();
    chris.sync();
    let mine = chris.engine.status().usage.clone();
    assert_eq!(mine.month, month_utc(walkrelay_core::now_ms()));
    assert!(mine.class_a > 0 && mine.class_b > 0, "{mine:?}");
    assert!(!mine.near_limit);
    // Chris's total includes Alex's reported operations too.
    let alex_only = alex.engine.status().usage.clone();
    assert!(mine.class_a > alex_only.class_a / 2, "{mine:?} vs {alex_only:?}");

    // Counts survive a restart (same data dir).
    let data = chris._tmp.path().join("data");
    let before: serde_json::Value = serde_json::from_slice(&std::fs::read(data.join("usage.json")).unwrap()).unwrap();
    assert!(before["class_a"].as_u64().unwrap() > 0);
}

#[test]
fn near_limit_warns_once_per_month() {
    use walkrelay_core::engine::month_utc;
    let store = MemoryStore::new();
    let mut alex = Friend::new("Alex", &store);
    alex.sync();
    // A friend reports heavy usage this month.
    let heavy = serde_json::json!({
        "player": "Heavy", "game_running": false, "hosting": null, "updated_at": 0,
        "usage": { "month": month_utc(walkrelay_core::now_ms()), "class_a": 750_000, "class_b": 0 }
    });
    // An older client's presence (no usage field) must still be read.
    let old = serde_json::json!({ "player": "Old", "game_running": false, "hosting": null, "updated_at": 0 });
    use walkrelay_core::{Cond, RemoteStore};
    store.put(&format!("v1/presence/{}.json", hex::encode("Heavy")), heavy.to_string().as_bytes(), Cond::None).unwrap();
    store.put(&format!("v1/presence/{}.json", hex::encode("Old")), old.to_string().as_bytes(), Cond::None).unwrap();

    let ev = alex.sync();
    assert!(ev.iter().any(|e| matches!(e, Event::NearFreeLimit { percent } if *percent >= 75)), "{ev:?}");
    let u = &alex.engine.status().usage;
    assert!(u.near_limit && u.class_a >= 750_000, "{u:?}");
    let ev = alex.sync();
    assert!(!ev.iter().any(|e| matches!(e, Event::NearFreeLimit { .. })), "warned twice: {ev:?}");

    // Last month's usage doesn't count.
    let stale = serde_json::json!({
        "player": "Heavy", "game_running": false, "hosting": null, "updated_at": 0,
        "usage": { "month": "2000-01", "class_a": 999_999, "class_b": 0 }
    });
    store.put(&format!("v1/presence/{}.json", hex::encode("Heavy")), stale.to_string().as_bytes(), Cond::None).unwrap();
    alex.sync();
    assert!(!alex.engine.status().usage.near_limit);
}

#[test]
fn month_is_computed_in_utc() {
    use walkrelay_core::engine::month_utc;
    assert_eq!(month_utc(0), "1970-01");
    assert_eq!(month_utc(951_782_400_000), "2000-02"); // 2000-02-29
    assert_eq!(month_utc(1_798_761_599_000), "2026-12"); // 2026-12-31 23:59:59
    assert_eq!(month_utc(1_798_761_600_000), "2027-01");
}
