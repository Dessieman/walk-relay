//! Runs the real S3/R2 client against a live bucket.
//! Skipped unless WALKRELAY_TEST_CODE is set to a group code, e.g.:
//!   WALKRELAY_TEST_CODE=WR1-... cargo test -p walkrelay-core --test s3_live -- --nocapture

use std::sync::Arc;
use walkrelay_core::{Cond, Engine, EngineOptions, GroupCode, PutOutcome, R2Store, RemoteStore};

fn store() -> Option<R2Store> {
    let code = std::env::var("WALKRELAY_TEST_CODE").ok()?;
    Some(R2Store::new(&GroupCode::decode(&code).unwrap()).unwrap())
}

#[test]
fn conditional_writes_are_enforced_by_the_server() {
    let Some(s) = store() else { eprintln!("skipped: WALKRELAY_TEST_CODE not set"); return };
    s.check_access().unwrap();
    let key = format!("v1/_test/cond-{}", walkrelay_core::now_ms());

    let PutOutcome::Ok { etag: e1 } = s.put(&key, b"one", Cond::IfNoneMatch).unwrap() else { panic!("first create failed") };
    assert_eq!(s.put(&key, b"two", Cond::IfNoneMatch).unwrap(), PutOutcome::PreconditionFailed, "If-None-Match ignored");
    let PutOutcome::Ok { etag: e2 } = s.put(&key, b"two", Cond::IfMatch(e1.clone())).unwrap() else { panic!("If-Match update failed") };
    assert_ne!(e1, e2);
    assert_eq!(s.put(&key, b"three", Cond::IfMatch(e1)).unwrap(), PutOutcome::PreconditionFailed, "stale If-Match accepted");

    let (data, etag) = s.get(&key).unwrap().unwrap();
    assert_eq!(data, b"two");
    assert_eq!(etag, e2);
    let listed = s.list("v1/_test/").unwrap();
    assert!(listed.iter().any(|o| o.key == key && o.etag == e2), "list etag must match put etag: {listed:?}");

    s.delete(&key).unwrap();
    assert!(s.get(&key).unwrap().is_none());
}

#[test]
fn two_friends_sync_through_the_real_bucket() {
    let Some(s) = store() else { eprintln!("skipped: WALKRELAY_TEST_CODE not set"); return };
    let s: Arc<dyn RemoteStore> = Arc::new(s);
    let stem = format!("save_live test_{:016x}", walkrelay_core::now_ms());
    let mk = |name: &str| {
        let tmp = tempfile::TempDir::new().unwrap();
        let saves = tmp.path().join("save_games");
        std::fs::create_dir_all(&saves).unwrap();
        let mut o = EngineOptions::new(name, tmp.path().join("data"));
        o.save_dirs_override = Some(vec![saves.clone()]);
        o.stable_ms = 0;
        (tmp, saves, Engine::new(s.clone(), o).unwrap())
    };
    let (_t1, a_dir, mut a) = mk("LiveA");
    let (_t2, b_dir, mut b) = mk("LiveB");
    std::fs::write(a_dir.join(format!("{stem}.sav")), "progress with spaces in name").unwrap();
    a.sync_once(false).unwrap();
    b.sync_once(false).unwrap();
    assert_eq!(std::fs::read_to_string(b_dir.join(format!("{stem}.sav"))).unwrap(), "progress with spaces in name");
    // Steady state: no churn.
    assert!(a.sync_once(false).unwrap().is_empty());
    assert!(b.sync_once(false).unwrap().is_empty());

    // Clean up the test journey for everyone.
    for o in s.list("v1/").unwrap() {
        if o.key.contains(&hex::encode(stem.as_bytes())) {
            s.delete(&o.key).unwrap();
        }
    }
}
