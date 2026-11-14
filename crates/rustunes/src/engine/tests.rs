use super::*;
use std::time::Duration;

#[test]
fn enclosing_deadline_and_cancellation_are_shared_without_renewal() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(5);
    let engine = Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: Some(deadline),
        cancelled: Arc::clone(&cancelled),
    });
    let first = engine.control(120);
    let second = engine.control(1800);
    assert_eq!(first.deadline, deadline);
    assert_eq!(second.deadline, deadline);
    assert!(Arc::ptr_eq(&first.cancelled, &cancelled));
    assert!(Arc::ptr_eq(&second.cancelled, &cancelled));
    cancelled.store(true, Ordering::Relaxed);
    assert!(first.check().is_err());
    assert!(engine.check().is_err());
}

#[tokio::test]
async fn nested_engine_observers_remain_scoped_to_their_caller() {
    let outer_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inner_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outer_sink = Arc::clone(&outer_count);
    let inner_sink = Arc::clone(&inner_count);
    let outer = Engine::new(test_engine().config, move |_| {
        outer_sink.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    let inner = Engine::new(test_engine().config, move |_| {
        inner_sink.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    outer
        .scope(async {
            observe(Event::ReadingSavedSnapshot).unwrap();
            inner
                .scope(async {
                    observe(Event::ComparingSources).unwrap();
                })
                .await;
            observe(Event::ReadingSavedSnapshot).unwrap();
        })
        .await;
    observe(Event::ComparingSources).unwrap();
    assert_eq!(outer_count.load(Ordering::Relaxed), 2);
    assert_eq!(inner_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn scope_keeps_large_composed_futures_off_the_task_stack() {
    let engine = Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    let future = async {
        let data = std::hint::black_box([7u8; 128 * 1024]);
        tokio::task::yield_now().await;
        std::hint::black_box(data).len()
    };
    let scoped = engine.scope(future);
    assert!(std::mem::size_of_val(&scoped) < 16 * 1024);
    assert_eq!(scoped.await, 128 * 1024);
}

#[test]
fn cancellation_follows_the_active_engine_scope_and_restores_its_parent() {
    let outer = Arc::new(AtomicBool::new(false));
    let inner = Arc::new(AtomicBool::new(false));
    let a = Engine::quiet(Config {
        device: Default::default(),
        deadline: None,
        cancelled: outer.clone(),
    });
    let b = Engine::quiet(Config {
        device: Default::default(),
        deadline: None,
        cancelled: inner.clone(),
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(a.scope(async {
        assert!(Arc::ptr_eq(&super::cancellation(), &outer));
        b.scope(async {
            assert!(Arc::ptr_eq(&super::cancellation(), &inner));
        })
        .await;
        outer.store(true, Ordering::Relaxed);
        assert!(super::cancellation().load(Ordering::Relaxed));
        assert!(!inner.load(Ordering::Relaxed));
    }));
    assert!(!super::cancellation().load(Ordering::Relaxed));
}

#[test]
#[ignore = "FLURB_FAILED_INSERT points to retained before/after snapshots; read-only"]
fn saved_insertion_prior_track_difference() {
    let root = std::path::PathBuf::from(std::env::var_os("FLURB_FAILED_INSERT").unwrap());
    let old = crate::engine::state::snapshot::open(&root.join("snapshot")).unwrap();
    let new = crate::engine::state::snapshot::open(&root.join("observe-001/snapshot")).unwrap();
    let id = std::env::var("FLURB_PRIOR_TRACK")
        .map(|v| u64::from_str_radix(&v, 16).unwrap())
        .unwrap_or(0x9c70071f27c54d08);
    let a = old
        .catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == id)
        .unwrap();
    let b = new
        .catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == id)
        .unwrap();
    eprintln!("BEFORE {a:#?}\nAFTER {b:#?}");
}

#[test]
#[ignore = "FLURB_FAILED_INSERT identifies an actual retained insert batch; offline verification only"]
fn saved_insertion_batch_verifies_without_resending() {
    use music_protocol::catalogue::insert_verification::{self, ExpectedInsert};
    let root = std::path::PathBuf::from(std::env::var_os("FLURB_FAILED_INSERT").unwrap());
    let before = crate::engine::state::snapshot::open(&root.join("snapshot")).unwrap();
    let after = crate::engine::state::snapshot::open(&root.join("observe-001/snapshot")).unwrap();
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("input.json")).unwrap()).unwrap();
    let rows = input["members"].as_array().unwrap();
    let sources:Vec<_>=rows.iter().map(|r|{let p=std::path::Path::new(r["source_path"].as_str().unwrap());let s=crate::engine::planning::source_reads::mpeg(p).unwrap();crate::engine::planning::source_collection::mp3::preparation::PreparedSource::read_expected(p,&s).unwrap()}).collect();
    let pid = |v: &serde_json::Value| u64::from_str_radix(v.as_str().unwrap(), 16).unwrap();
    let expected: Vec<_> = rows
        .iter()
        .zip(&sources)
        .map(|(r, s)| ExpectedInsert {
            track_id: pid(&r["track"]),
            album_id: pid(&r["album"]),
            artist_id: pid(&r["artist"]),
            path: r["path"].as_str().unwrap(),
            file_bytes: r["file_bytes"].as_u64().unwrap(),
            metadata: s.expected().unwrap(),
            presented: true,
        })
        .collect();
    let covers = rows
        .iter()
        .filter_map(|r| {
            r["cover"]["cache_id"]
                .as_u64()
                .map(|id| (pid(&r["track"]), id as u32))
        })
        .collect();
    let result = insert_verification::verify_insert_batch_covered(
        &before.catalog,
        &after.catalog,
        &expected,
        &Default::default(),
        &Default::default(),
        &covers,
    )
    .unwrap();
    assert!(
        result
            .case_renamed
            .iter()
            .any(|r| r.kind.starts_with("track_"))
    );
    eprintln!(
        "Verified {} inserted tracks against the retained failure, without resending",
        expected.len()
    );
}
