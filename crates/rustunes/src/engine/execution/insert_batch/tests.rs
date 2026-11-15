//! The add-only album-image check, without a phone: a connection that fails
//! stands in for one that cannot finish.
use super::*;
use crate::engine::state::current::Current;
use sha2::Digest;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn current(catalog: music_protocol::catalogue::sqlite::Catalog) -> Current {
    let mut prefs = vec![0u8; 1232];
    prefs[..4].copy_from_slice(b"frpd");
    prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
    prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
    prefs[8] = 1;
    prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
    prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
    let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
    Current {
        device: "synthetic-device".into(),
        identity: Identity::new("synthetic-device", &library).unwrap(),
        library,
        catalog,
        raw_preferences: prefs.clone(),
        plist_preferences: prefs,
        cdb: vec![],
        artwork_db: None,
        receipt_sha256: [1; 32],
    }
}

fn cover(bytes: &[u8]) -> Cover {
    Cover {
        sha256: sha2::Sha256::digest(bytes).into(),
        checksum: bytes.len() as u32 + 1,
        bytes: bytes.to_vec(),
    }
}

fn member(name: &str, album: u64, cover: Option<Cover>, keep_album_image: bool) -> Member {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tone-alac.m4a");
    Member {
        relative_path: name.into(),
        audio: Audio::Alac {
            fingerprint: music_protocol::media::fingerprint::read(&source).unwrap(),
            metadata: metadata::read(&source).unwrap(),
        },
        source,
        track: 1,
        album,
        artist: 2,
        destination: format!("iTunes_Control/Music/F00/{name}.m4a"),
        cover,
        keep_album_image,
        conversion: None,
        convert: false,
    }
}

fn control(cancelled: bool) -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(600),
        cancelled: Arc::new(AtomicBool::new(cancelled)),
    }
}

struct Temp(PathBuf);
impl Temp {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "insert-batch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn a_cover_whose_album_shows_no_image_is_left_out() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let album = catalog.tracks()[0].album_sync_id;
    let current = current(catalog);
    let temp = Temp::new("no-image");
    let connects = AtomicUsize::new(0);
    let connect = || async {
        connects.fetch_add(1, Ordering::Relaxed);
        bail!("no phone in a unit test")
    };
    let mut chunk = [
        member("first", album, Some(cover(b"front")), true),
        // The same album and cover: the first finding serves.
        member("second", album, Some(cover(b"front")), true),
        // Not add-only: its cover rides as planned.
        member("plain", album, Some(cover(b"front")), false),
        member("bare", album, None, true),
    ];
    let left: Vec<String> =
        keep_album_images(&current, &temp.0, &mut chunk, &control(false), connect)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.relative_path.clone())
            .collect();
    assert_eq!(left, ["first", "second"]);
    assert!(chunk[0].cover.is_none() && chunk[1].cover.is_none());
    assert!(chunk[2].cover.is_some());
    // No image to read: the phone is never asked.
    assert_eq!(connects.load(Ordering::Relaxed), 0);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.0.join("album-images.json")).unwrap()).unwrap();
    let record = record.as_array().unwrap();
    assert_eq!(record.len(), 2);
    for (entry, source) in record.iter().zip(["first", "second"]) {
        assert_eq!(entry["source"], source);
        assert_eq!(entry["album_image_equal"], false);
        assert_eq!(entry["reason"], "supported source300 registration absent");
    }
}

#[tokio::test]
async fn a_read_that_cannot_finish_stops_before_the_batch() {
    let catalog = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
    let album = catalog.tracks()[0].album_sync_id;
    let current = current(catalog);
    for cancelled in [false, true] {
        let temp = Temp::new(&format!("unread-{cancelled}"));
        let mut chunk = [member("joins", album, Some(cover(b"front")), true)];
        let connect = || async { bail!("no phone in a unit test") };
        let Err(error) =
            keep_album_images(&current, &temp.0, &mut chunk, &control(cancelled), connect).await
        else {
            panic!("the read cannot finish");
        };
        assert!(
            format!("{error:#}").contains("nothing was sent"),
            "{error:#}"
        );
        // Nothing is decided: the cover stays, and no finding is recorded.
        assert!(chunk[0].cover.is_some());
        assert!(!temp.0.join("album-images.json").exists());
    }
}

#[tokio::test]
async fn an_album_missing_from_the_capture_stops_the_run() {
    let current = current(music_protocol::test_support::catalogue::artwork_catalog(
        677, 678,
    ));
    let temp = Temp::new("absent");
    let mut chunk = [member("joins", 999, Some(cover(b"front")), true)];
    let connect = || async { bail!("no phone in a unit test") };
    assert!(
        keep_album_images(&current, &temp.0, &mut chunk, &control(false), connect)
            .await
            .is_err()
    );
    assert!(chunk[0].cover.is_some());
}

/// A 0.2 s stereo tone FLAC FFmpeg makes in `temp` with `audio` options,
/// with a JPEG front cover when `covered`.
fn flac(temp: &Temp, name: &str, audio: &[&str], covered: bool) -> PathBuf {
    let ffmpeg = |args: &[&str]| {
        let output = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-v", "error", "-n"])
            .args(args)
            .env_remove("FFREPORT")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let path = temp.0.join(name);
    let cover = temp.0.join("cover.jpg");
    if covered && !cover.exists() {
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=64x48",
            "-frames:v",
            "1",
            cover.to_str().unwrap(),
        ]);
    }
    let mut args = vec![
        "-f",
        "lavfi",
        "-t",
        "0.2",
        "-i",
        "sine=frequency=440:sample_rate=44100",
    ];
    if covered {
        args.extend([
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:v",
            "copy",
        ]);
        args.extend([
            "-disposition:v:0",
            "attached_pic",
            "-metadata:s:v",
            "comment=Cover (front)",
        ]);
    }
    args.extend(["-ac", "2"]);
    args.extend(audio);
    args.extend([
        "-c:a",
        "flac",
        "-map_metadata",
        "-1",
        "-metadata",
        "title=Tone",
    ]);
    args.extend(["-metadata", "artist=Artist", "-metadata", "album=Album"]);
    args.push(path.to_str().unwrap());
    ffmpeg(&args);
    path
}

/// A FLAC member as `members` makes it: read, with its cover as sent, still
/// to be converted.
fn pending(path: &Path) -> Member {
    let source = music_protocol::media::mpeg::read(path).unwrap();
    let prepared = PreparedSource::read_expected(path, &source).unwrap();
    let cover =
        crate::engine::planning::source_collection::mp3::artwork::batch_cover(path, &source)
            .unwrap();
    let name = path.file_name().unwrap().to_str().unwrap().to_owned();
    Member {
        destination: format!("iTunes_Control/Music/F00/{name}.m4a"),
        relative_path: name,
        source: path.to_owned(),
        audio: Audio::Mpeg(Arc::new(prepared)),
        track: 3,
        album: 4,
        artist: 5,
        cover,
        keep_album_image: false,
        conversion: None,
        convert: true,
    }
}

#[tokio::test]
async fn a_batch_converts_its_flacs_and_goes_on_without_a_refused_one() {
    let temp = Temp::new("convert");
    let song = flac(&temp, "song.flac", &[], true);
    let deep = flac(
        &temp,
        "deep.flac",
        &["-sample_fmt", "s32", "-strict", "experimental"],
        false,
    );
    let sent_flac_cover = pending(&song).cover.unwrap();
    let mut chunk = vec![
        pending(&deep),
        member("plain", 7, None, false),
        pending(&song),
    ];
    let converter = flac_to_alac::Converter {
        ffmpeg: flac_to_alac::require_ffmpeg().unwrap(),
        directory: flac_to_alac::Directory::create().unwrap(),
    };
    let mut conversions =
        lookahead::Lookahead::ffmpeg(&chunk, 3, &converter, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .unwrap();
    let mut refusals = Vec::new();
    let sendable = conversions
        .batch(&mut chunk, 0, &mut |member, error| {
            refusals.push((member.relative_path.clone(), format!("{error:#}")));
            Ok(())
        })
        .await
        .unwrap();
    // The batch took every conversion: the run's end removes none.
    drop(conversions);
    // The refused FLAC goes last; the others keep their order.
    assert_eq!(sendable, 2);
    assert_eq!(
        chunk
            .iter()
            .map(|m| m.relative_path.as_str())
            .collect::<Vec<_>>(),
        ["plain", "song.flac", "deep.flac"]
    );
    assert!(
        matches!(&refusals[..], [(name, why)] if name == "deep.flac" && why.contains("32-bit"))
    );
    assert!(chunk[2].convert && chunk[2].conversion.is_none());
    let converted = &chunk[1];
    assert!(!converted.convert);
    assert!(converted.source.starts_with(converter.directory.path()));
    assert!(matches!(&converted.audio, Audio::Alac { metadata, .. } if metadata.title == ["Tone"]));
    assert_eq!(
        converted.conversion.as_ref().unwrap().source_path,
        std::fs::canonicalize(&song).unwrap()
    );
    // Its binding records the FLAC, which outlasts the ALAC sent.
    assert_eq!(
        converted.recorded_source(),
        std::fs::canonicalize(&song).unwrap()
    );
    assert_eq!(chunk[0].recorded_source(), chunk[0].source);
    // The same cover, now with an M4A file's checksum.
    let cover = converted.cover.as_ref().unwrap();
    assert_eq!(cover.sha256, sent_flac_cover.sha256);
    assert_eq!(sent_flac_cover.checksum, cover.bytes.len() as u32 + 1);
    assert_eq!(cover.checksum, cover.bytes.len() as u32 + 17);
    // Its batch settled: the conversion goes, and the directory with the run.
    // Nothing outside a conversion directory is ever removed.
    let failure = flac_to_alac::remove(&song).err().unwrap();
    assert!(format!("{failure:#}").contains("not a conversion"));
    assert!(song.exists());
    flac_to_alac::remove(&converted.source).unwrap();
    assert!(!converted.source.exists());
    let directory = converter.directory.path().to_owned();
    converter.directory.finish().unwrap();
    assert!(!directory.exists());
}

#[test]
fn uploads_share_connections_while_their_allowances_fit() {
    use music_protocol::protocol::transfer::MAX_BOUNDED_OPS;
    // Uploads run last member first; each connection opens at its first upload.
    assert_eq!(connections(&[100, 200, 300]), vec![None, None, Some(600)]);
    let big = MAX_BOUNDED_OPS / 2;
    assert_eq!(
        connections(&[big, big, 10, big]),
        vec![None, Some(2 * big), None, Some(big + 10)]
    );
    assert_eq!(
        connections(&[MAX_BOUNDED_OPS, MAX_BOUNDED_OPS]),
        vec![Some(MAX_BOUNDED_OPS), Some(MAX_BOUNDED_OPS)]
    );
    assert!(connections(&[]).is_empty());
}

#[test]
#[ignore = "explicit FLURB_INSERT_REPLAY directory; reads retained captures and sources only"]
fn replay_retained_insert_catalogue() {
    let root =
        PathBuf::from(std::env::var_os("FLURB_INSERT_REPLAY").expect("explicit retained batch"));
    let before = snapshot::open(&root.join("snapshot")).unwrap();
    let observation =
        std::env::var("FLURB_INSERT_OBSERVE").unwrap_or_else(|_| "observe-001".into());
    let after = snapshot::open(&root.join(observation).join("snapshot")).unwrap();
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("input.json")).unwrap()).unwrap();
    let members = input["members"].as_array().unwrap();
    let sources: Vec<_> = members
        .iter()
        .map(|m| {
            Audio::Mpeg(Arc::new(
                PreparedSource::read_expected(
                    Path::new(m["source_path"].as_str().unwrap()),
                    &crate::engine::planning::source_reads::mpeg(Path::new(
                        m["source_path"].as_str().unwrap(),
                    ))
                    .unwrap(),
                )
                .unwrap(),
            ))
        })
        .collect();
    let number = |m: &serde_json::Value, key: &str| {
        u64::from_str_radix(m[key].as_str().unwrap(), 16).unwrap()
    };
    let expected: Vec<_> = members
        .iter()
        .zip(&sources)
        .map(|(m, s)| ExpectedInsert {
            track_id: number(m, "track"),
            album_id: number(m, "album"),
            artist_id: number(m, "artist"),
            path: m["path"].as_str().unwrap(),
            file_bytes: m["file_bytes"].as_u64().unwrap(),
            metadata: s.expected().unwrap(),
            presented: true,
        })
        .collect();
    for (member, source) in members.iter().zip(&sources) {
        assert_eq!(
            hex(&source.transferred().full_file_sha256),
            member["full_source_sha256"].as_str().unwrap()
        );
    }
    let targets = expected.iter().map(|e| e.track_id).collect();
    prior_unchanged(&before, &after, &targets, &BTreeSet::new()).unwrap();
    let covers = members
        .iter()
        .filter_map(|m| {
            m["cover"]["cache_id"]
                .as_u64()
                .map(|id| (number(m, "track"), id as u32))
        })
        .collect();
    let checked = insert_verification::verify_insert_batch_after_listening(
        &before.catalog,
        &after.catalog,
        &expected,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &covers,
        &between_runs::catalogue_listening,
    )
    .unwrap();
    eprintln!(
        "Verified {} inserted targets, {} prior tracks, {} native artist identity adoptions and {} album identity adoptions",
        checked.track_ids.len(),
        checked.prior_tracks_preserved,
        checked.artist_identity_adoptions.len(),
        checked.album_identity_adoptions.len()
    );
}

#[test]
#[ignore = "explicit FLURB_PROFILE_INPUT; read-only phone capture and offline batch planning"]
fn profile_live_batch_preparation_without_sending() {
    let input = PathBuf::from(std::env::var_os("FLURB_PROFILE_INPUT").expect("input"));
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let temp = Temp::new("plan-profile");
            let engine = crate::engine::Engine::new(
                crate::engine::Config {
                    device: Default::default(),
                    deadline: None,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
                |_| Ok(()),
            )
            .with_source_cache(PathBuf::from(
                "/Users/joey/.local/state/rustunes/source-cache",
            ))
            .with_fingerprint_cache(crate::engine::FingerprintCache::new(
                PathBuf::from("/Users/joey/.local/state/rustunes/fingerprint-cache"),
                crate::engine::FingerprintCacheMode::Reuse,
            ));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(engine.scope(async {
                let control = control(false);
                let t = std::time::Instant::now();
                let (_, before) = removal_batch::capture_fresh(&temp.0.join("before"), &control)
                    .await
                    .unwrap();
                eprintln!("PROFILE capture {:?}", t.elapsed());
                let data: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
                let t = std::time::Instant::now();
                let mut planner = Planner::new(&before.current.catalog, &BTreeSet::new());
                let mut members = Vec::new();
                for (index, m) in data["members"].as_array().unwrap().iter().enumerate() {
                    let path = PathBuf::from(m["source_path"].as_str().unwrap());
                    let source = PreparedSource::read_expected(
                        &path,
                        &crate::engine::planning::source_reads::mpeg(&path).unwrap(),
                    )
                    .unwrap();
                    let base = 0x1234_0000_0000_0000 + index as u64 * 3;
                    let selected = planner
                        .plan_mpeg(
                            &source,
                            CandidateIds {
                                track: base,
                                album: Some(base + 1),
                                artist: Some(base + 2),
                            },
                        )
                        .unwrap();
                    members.push(Member {
                        relative_path: m["source"].as_str().unwrap().into(),
                        source: path,
                        audio: Audio::Mpeg(Arc::new(source)),
                        track: selected.track_pid(),
                        album: selected.album_pid(),
                        artist: selected.artist_pid(),
                        destination: format!("iTunes_Control/Music/F00/T{index:03}.mp3"),
                        cover: None,
                        keep_album_image: false,
                        conversion: None,
                        convert: false,
                    });
                }
                eprintln!("PROFILE source preparation {:?}", t.elapsed());
                let t = std::time::Instant::now();
                let held = hold(&members).unwrap();
                eprintln!("PROFILE hold {:?}", t.elapsed());
                let journal =
                    Journal::create(&temp.0.join("journal"), &before.current.identity).unwrap();
                let t = std::time::Instant::now();
                let (op, _) =
                    plan(&before, &journal, &members, [1; 32], SystemTime::UNIX_EPOCH).unwrap();
                eprintln!(
                    "PROFILE plan {:?}, operation bytes {}",
                    t.elapsed(),
                    op.len()
                );
                drop(held);
                for member in &mut members {
                    member.cover=Some(Cover { bytes:vec![1;1024],checksum:1,sha256:sha(&vec![1;1024]) });
                }
                let t=std::time::Instant::now();
                let _=plan(&before,&journal,&members,[1;32],SystemTime::UNIX_EPOCH).unwrap();
                eprintln!("PROFILE plan with covers {:?}",t.elapsed());
                let real_path=PathBuf::from("/Users/joey/.local/state/rustunes/devices/6f380ba21134c26538a4444f0be58c0e1fd2a77c4e0ef2eaf5d7e580578145f1/b0356ad1f219b854ccd262c53f4a48edbfc655794e2b3eaf870c2c39fe5b1ee7/journal");
                let t=std::time::Instant::now();
                let real=Journal::open_read_only(&real_path,&before.current.identity).unwrap();
                eprintln!("PROFILE real journal open {:?}",t.elapsed());
                let t=std::time::Instant::now();
                let _=plan(&before,&real,&members,[1;32],SystemTime::UNIX_EPOCH).unwrap();
                eprintln!("PROFILE plan with real journal {:?}",t.elapsed());
                drop(real);
                drop(journal);
                let checks:Vec<_>=before.current.catalog.artwork().album_cache_ids().keys().take(64).enumerate().map(|(i,&album)|
                    crate::engine::verification::artwork::AlbumOriginalCheck {album,
                        expected:crate::engine::verification::artwork::OriginalArtworkExpectation { bytes:1024,sha256:[1;32] },
                        trace:temp.0.join(format!("album-profile-{i:03}")) }).collect();
                let read_control=Control {deadline:Instant::now()+Duration::from_secs(120),cancelled:Arc::clone(&control.cancelled)};
                let t=std::time::Instant::now();
                for (i,check) in checks.iter().take(4).enumerate() {
                    let _ = crate::engine::verification::artwork::album_original_equal(&before.current,check.album,&check.expected,
                        &temp.0.join(format!("album-single-{i}")),&read_control,device::provider).await.unwrap();
                }
                eprintln!("PROFILE four single album checks {:?}",t.elapsed());
                let t=std::time::Instant::now();
                let compared=crate::engine::verification::artwork::album_originals_equal_group(&before.current,&checks,
                    &temp.0.join("album-group"),&read_control,device::provider).await.unwrap();
                eprintln!("PROFILE {} grouped album checks {:?}",compared.len(),t.elapsed());

                let t = std::time::Instant::now();
                let _next =
                    removal_batch::retain_before(before, &temp.0.join("next"), &control).unwrap();
                eprintln!("PROFILE retain {:?}", t.elapsed());
            }));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn maximum_protocol_batch_keeps_all_source_handles_pinned() {
    let members: Vec<_> = (0..499)
        .map(|i| member(&format!("large-{i}"), 1, None, false))
        .collect();
    let held = hold(&members).unwrap();
    assert_eq!(held.len(), 499);
    for source in &held {
        source.check().unwrap();
    }
}
