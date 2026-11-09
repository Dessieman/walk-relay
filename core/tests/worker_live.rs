//! The sync engine running through a real (locally served) Walk Relay Worker.
//! Skipped unless WALKRELAY_TEST_WORKER is set to `{"url":..., "token":...}`
//! as printed by `node worker/test/serve.mjs`.

use std::sync::Arc;
use std::time::{Duration, SystemTime};
use walkrelay_core::engine::{list_deleted, list_versions, restore_version};
use walkrelay_core::{Engine, EngineOptions, Event, RemoteStore, VersionKind, WorkerClient};

fn owner() -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::env::var("WALKRELAY_TEST_WORKER").ok()?).ok()?;
    Some((v["url"].as_str()?.to_string(), v["token"].as_str()?.to_string()))
}

struct Friend {
    _tmp: tempfile::TempDir,
    saves: std::path::PathBuf,
    engine: Engine,
    store: Arc<WorkerClient>,
}

fn friend(url: &str, owner_token: &str, name: &str) -> Friend {
    let owner = WorkerClient::new(url, owner_token).unwrap();
    let invite = owner.create_invite().unwrap();
    let (token, member) = WorkerClient::redeem(url, &invite.id, name).unwrap();
    assert_eq!(member.name, name);
    let store = Arc::new(WorkerClient::new(url, &token).unwrap());
    let tmp = tempfile::TempDir::new().unwrap();
    let saves = tmp.path().join("save_games");
    std::fs::create_dir_all(&saves).unwrap();
    let mut o = EngineOptions::new(name, tmp.path().join("data"));
    o.save_dirs_override = Some(vec![saves.clone()]);
    o.stable_ms = 0;
    o.stable_ms_while_running = 0;
    o.delete_confirm_ms = 0;
    let engine = Engine::new(store.clone(), o).unwrap();
    Friend { _tmp: tmp, saves, engine, store }
}

fn play(f: &Friend, stem: &str, s: &str, secs_ago: u64) {
    for (ext, v) in [(".sav", s.to_string()), (".sav.bak", format!("{s}-bak"))] {
        let p = f.saves.join(format!("{stem}{ext}"));
        std::fs::write(&p, v).unwrap();
        let file = std::fs::File::options().write(true).open(&p).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
    }
}
fn read(f: &Friend, stem: &str) -> Option<String> {
    std::fs::read_to_string(f.saves.join(format!("{stem}.sav"))).ok()
}

#[test]
fn friends_sync_conflict_delete_and_restore_through_the_worker() {
    let Some((url, token)) = owner() else { eprintln!("skipped: WALKRELAY_TEST_WORKER not set"); return };
    let stem = format!("save_worker walk_{:016x}", walkrelay_core::now_ms());
    let mut a = friend(&url, &token, "Alex");
    let mut b = friend(&url, &token, "Blair");

    // Spread, then steady state.
    play(&a, &stem, "v1", 60);
    a.engine.sync_once(false).unwrap();
    let ev = b.engine.sync_once(false).unwrap();
    assert!(ev.iter().any(|e| matches!(e, Event::Downloaded { .. })), "{ev:?}");
    assert_eq!(read(&b, &stem).as_deref(), Some("v1"));
    assert!(a.engine.sync_once(false).unwrap().is_empty());

    // Both play offline: the newer session wins, the other is kept as a conflict copy.
    play(&a, &stem, "alex-old", 3600);
    play(&b, &stem, "blair-new", 30);
    a.engine.sync_once(false).unwrap();
    let ev = b.engine.sync_once(false).unwrap();
    assert!(ev.iter().any(|e| matches!(e, Event::Conflict { kept_from, .. } if kept_from == "Blair")), "{ev:?}");
    a.engine.sync_once(false).unwrap();
    assert_eq!(read(&a, &stem).as_deref(), Some("blair-new"));
    let versions = list_versions(a.store.as_ref(), &stem).unwrap();
    assert!(versions.iter().any(|v| v.kind == VersionKind::Conflict && v.player == "Alex"));

    // Many uploads: the Worker keeps history at 20 and the client never deletes it.
    for i in 0..23 {
        play(&a, &stem, &format!("h{i}"), 0);
        a.engine.sync_once(false).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    let hist = list_versions(a.store.as_ref(), &stem).unwrap().into_iter().filter(|v| v.kind == VersionKind::History).count();
    assert_eq!(hist, 20);
    assert!(a.engine.status().last_error.is_none(), "{:?}", a.engine.status().last_error);

    // Delete in game -> deleted for everyone -> restorable.
    for ext in [".sav", ".sav.bak"] {
        std::fs::remove_file(a.saves.join(format!("{stem}{ext}"))).unwrap();
    }
    a.engine.sync_once(false).unwrap();
    a.engine.sync_once(false).unwrap();
    b.engine.sync_once(false).unwrap();
    assert!(read(&b, &stem).is_none());
    assert!(list_deleted(b.store.as_ref()).unwrap().iter().any(|(s, _)| *s == stem));
    let v = &list_versions(b.store.as_ref(), &stem).unwrap()[0];
    restore_version(b.store.as_ref(), &stem, &v.key, "Blair").unwrap();
    b.engine.sync_once(false).unwrap();
    a.engine.sync_once(false).unwrap();
    assert!(read(&a, &stem).is_some() && read(&b, &stem).is_some());
}

#[test]
fn removed_member_is_cut_off_and_invites_are_single_use() {
    let Some((url, token)) = owner() else { eprintln!("skipped: WALKRELAY_TEST_WORKER not set"); return };
    let owner = WorkerClient::new(&url, &token).unwrap();
    let inv = owner.create_invite().unwrap();
    assert!(inv.url.ends_with(&format!("/j/{}", inv.id)));
    let (t, m) = WorkerClient::redeem(&url, &inv.id, "Temp").unwrap();
    let err = WorkerClient::redeem(&url, &inv.id, "Again").unwrap_err().to_string();
    assert!(err.contains("already been used"), "{err}");
    let temp = WorkerClient::new(&url, &t).unwrap();
    assert!(temp.list("v1/journeys/").is_ok());
    assert!(temp.remove_member(&owner.me().unwrap().id).is_err(), "non-owner can't remove");
    owner.remove_member(&m.id).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(temp.list("v1/journeys/").is_err());
    assert!(owner.members().unwrap().iter().all(|x| x.name != "Temp"));
}
