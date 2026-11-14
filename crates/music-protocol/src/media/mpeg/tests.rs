use super::*;
use std::path::PathBuf;
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mp3-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn read(&self, bytes: &[u8]) -> Result<Source> {
        let path = self.0.join("test.mp3");
        fs::write(&path, bytes)?;
        read(&path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub(crate) use super::test_fixture::audio;
fn safe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 127) as u8,
        ((n >> 14) & 127) as u8,
        ((n >> 7) & 127) as u8,
        (n & 127) as u8,
    ]
}
fn frame(id: &str, data: &[u8], v: u8) -> Vec<u8> {
    [
        id.as_bytes(),
        if v == 4 {
            safe(data.len())
        } else {
            (data.len() as u32).to_be_bytes()
        }
        .as_slice(),
        &[0, 0],
        data,
    ]
    .concat()
}
fn tagged(frames: Vec<Vec<u8>>, v: u8) -> Vec<u8> {
    let frames = frames.concat();
    [
        b"ID3".as_slice(),
        &[v, 0, 0],
        &safe(frames.len()),
        frames.as_slice(),
        audio().as_slice(),
    ]
    .concat()
}
#[test]
fn real_frame_bytes_identify_tag_variants_and_distinguish_audio_changes() {
    let t = Temp::new();
    let a = tagged(vec![frame("TIT2", b"\x03Track", 4)], 4);
    let b = tagged(vec![frame("TIT2", b"\x03TRACK\0alternate", 4)], 4);
    let x = t.read(&a).unwrap();
    let y = t.read(&b).unwrap();
    assert_ne!(
        x.fingerprint.full_file_sha256,
        y.fingerprint.full_file_sha256
    );
    assert_eq!(x.fingerprint.audio, y.fingerprint.audio);
    assert_eq!(y.metadata.title, ["TRACK", "alternate"]);
    let mut c = b;
    c.last_mut().map(|b| *b = 1);
    assert_ne!(t.read(&c).unwrap().fingerprint.audio, x.fingerprint.audio);
    assert_eq!(x.fingerprint.audio.coded_samples, 2304);
    assert_eq!(
        x.fingerprint.audio.sha256,
        "0e9ea8a196febd7d8097c3eff0ef4f4bce3f681ebadb0851b390d8117494adba"
    );
    assert_eq!(
        x.fingerprint.audio.frame_bytes_sha256,
        "ade453b6d5fc720acc24dd0602d5b5e086f56cb7c50190f4641a76d14014ce3a"
    );
    assert_eq!(x.timing, y.timing);
    assert_eq!(
        x.frame_timing().unwrap().physical.coded_duration.samples,
        2304
    );
    // Descriptive timing is not part of either encoded identity or the
    // transferred-source value that journal Content serializes.
    let original = x.transferred_identity().unwrap();
    let mut altered_description = x;
    altered_description
        .timing
        .as_mut()
        .unwrap()
        .physical
        .coded_duration
        .samples = 1;
    assert!(altered_description.transferred_identity().unwrap() == original);
}
#[test]
fn id3_versions_unicode_and_numeric_arrays_are_real_not_mp4_fields() {
    let t = Temp::new();
    let bytes = tagged(
        vec![
            frame("TIT2", &[1, 0xff, 0xfe, b'E', 0, 0xac, 0x20], 3),
            frame("TRCK", b"\x001/2", 3),
            frame("TPOS", b"\x001/1", 3),
        ],
        3,
    );
    let s = t.read(&bytes).unwrap();
    assert_eq!(s.metadata.title, ["E€"]);
    assert_eq!(
        (s.metadata.track, s.metadata.track_total),
        (Some(1), Some(2))
    );
    assert!(serde_json::to_value(s).unwrap().get("bit_depth").is_none());
}
#[test]
fn an_empty_comment_says_nothing_and_a_written_one_is_kept() {
    let t = Temp::new();
    let empty = |data: &[u8], v| t.read(&tagged(vec![frame("COMM", data, v)], v)).unwrap();
    // Latin-1, UTF-8 with a text terminator, and UTF-16 with a BOM-less empty
    // description: none of them says anything.
    for (data, v) in [
        (b"\0eng\0".as_slice(), 4),
        (b"\x03eng\0\0".as_slice(), 4),
        (b"\x01eng\0\0".as_slice(), 3),
    ] {
        let s = empty(data, v);
        assert!(s.metadata.comments.is_empty(), "{data:?}");
        assert_eq!(s.frames[0].id, "COMM");
        assert!(
            serde_json::to_value(&s.metadata)
                .unwrap()
                .get("comments")
                .is_none()
        );
    }
    let s = empty(b"\0engiTunNORM\0 0000", 4);
    assert_eq!(
        s.metadata.comments,
        [Comment {
            language: "eng".into(),
            description: "iTunNORM".into(),
            text: " 0000".into(),
        }]
    );
    let s = empty(
        &[
            1, b'e', b'n', b'g', 0xff, 0xfe, 0, 0, 0xff, 0xfe, b'h', 0, b'i', 0,
        ],
        3,
    );
    assert_eq!(s.metadata.comments[0].text, "hi");
    for bad in [b"\0en".as_slice(), b"\0e1g\0", b"\0engno terminator"] {
        assert!(t.read(&tagged(vec![frame("COMM", bad, 4)], 4)).is_err());
    }
}
#[test]
fn an_empty_numeric_frame_is_no_number() {
    let t = Temp::new();
    let title = || frame("TIT2", b"\x03Title", 4);
    let plain = t.read(&tagged(vec![title()], 4)).unwrap();
    // UTF-8 with no text, and Latin-1 with a bare terminator.
    for data in [b"\x03".as_slice(), b"\x00\x00"] {
        let s = t
            .read(&tagged(
                vec![
                    title(),
                    frame("TRCK", data, 4),
                    frame("TPOS", data, 4),
                    frame("TCMP", data, 4),
                ],
                4,
            ))
            .unwrap();
        assert_eq!(s.metadata, plain.metadata, "{data:?}");
    }
    // An empty value beside a number drops out; two numbers stay ambiguous.
    let s = t.read(&tagged(vec![frame("TRCK", b"\x03\x005", 4)], 4));
    assert_eq!(s.unwrap().metadata.track, Some(5));
    let two = t.read(&tagged(vec![frame("TRCK", b"\x035\x006", 4)], 4));
    assert!(format!("{:#}", two.unwrap_err()).contains("ambiguous ID3 numeric frame"));
}
#[test]
fn malformed_flags_extents_duplicates_and_audio_frames_refuse() {
    let t = Temp::new();
    let mut flags = tagged(vec![], 4);
    flags[5] = 0x80;
    let mut broken = audio();
    broken.pop();
    let duplicate = tagged(
        vec![frame("TRCK", b"\x001", 4), frame("TRCK", b"\x002", 4)],
        4,
    );
    let mut extent = tagged(vec![frame("TIT2", b"\x03A", 4)], 4);
    extent[17] = 127;
    for bytes in [flags, broken, duplicate, extent, vec![0; 834]] {
        assert!(t.read(&bytes).is_err());
    }
}
#[test]
fn apic_validates_exact_jpeg_and_has_no_native_checksum() {
    let t = Temp::new();
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 0, 2, 0, 3, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0xff,
        0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0, 0, 0xff, 0xd9,
    ];
    let apic = [b"\x00image/jpeg\x00\x03\x00".as_slice(), &jpeg].concat();
    let s = t.read(&tagged(vec![frame("APIC", &apic, 4)], 4)).unwrap();
    let p = &s.artwork[0];
    assert_eq!((p.width, p.height), (3, 2));
    assert_eq!(p.sha256, hex(&Sha256::digest(jpeg)));
    assert!(!p.image_decode_verified);
    assert!(
        serde_json::to_value(p)
            .unwrap()
            .get("native_checksum")
            .is_none()
    );
    let captured = read_front_cover(&t.0.join("test.mp3"), &s)
        .unwrap()
        .unwrap();
    assert_eq!(captured.bytes(), jpeg);
    assert_eq!(captured.facts(), p);
    let duplicated = t
        .read(&tagged(
            vec![frame("APIC", &apic, 4), frame("APIC", &apic, 4)],
            4,
        ))
        .unwrap();
    // Two front covers: none is chosen, and the file goes without one.
    assert!(
        read_front_cover(&t.0.join("test.mp3"), &duplicated)
            .unwrap()
            .is_none()
    );
    let why = cover_left_out(&duplicated.artwork).unwrap();
    assert!(
        why.contains("2 pictures marked as the front cover"),
        "{why}"
    );
    // A lone picture is the cover whatever its type; beside other pictures,
    // the one front cover is.
    let other = [b"\x00image/jpeg\x00\x00\x00".as_slice(), &jpeg].concat();
    let lone = t.read(&tagged(vec![frame("APIC", &other, 4)], 4)).unwrap();
    assert_eq!(lone.artwork[0].picture_type, 0);
    assert_eq!(cover_index(&lone.artwork), Some(0));
    assert_eq!(
        read_front_cover(&t.0.join("test.mp3"), &lone)
            .unwrap()
            .unwrap()
            .bytes(),
        jpeg
    );
    let back = [b"\x00image/jpeg\x00\x04\x00".as_slice(), &jpeg].concat();
    let mixed = t
        .read(&tagged(
            vec![frame("APIC", &back, 4), frame("APIC", &apic, 4)],
            4,
        ))
        .unwrap();
    assert_eq!(cover_index(&mixed.artwork), Some(1));
    assert!(cover_left_out(&mixed.artwork).is_none());
    let unmarked = t
        .read(&tagged(
            vec![frame("APIC", &back, 4), frame("APIC", &other, 4)],
            4,
        ))
        .unwrap();
    assert_eq!(cover_index(&unmarked.artwork), None);
    assert!(
        cover_left_out(&unmarked.artwork)
            .unwrap()
            .contains("none is marked as the front cover")
    );
    let mut changed = tagged(vec![frame("APIC", &apic, 4)], 4);
    let last = changed.len() - 1;
    changed[last] ^= 1;
    fs::write(t.0.join("test.mp3"), changed).unwrap();
    assert!(read_front_cover(&t.0.join("test.mp3"), &s).is_err());
    let mut bad = apic;
    bad.pop();
    assert!(t.read(&tagged(vec![frame("APIC", &bad, 4)], 4)).is_err());
}
#[test]
fn apic_reads_png_and_any_jpeg_kind_by_their_own_bytes() {
    let t = Temp::new();
    let read = |mime: &[u8], image: &[u8]| {
        let apic = [b"\x00", mime, b"\x00\x03\x00", image].concat();
        t.read(&tagged(vec![frame("APIC", &apic, 4)], 4))
    };
    let png = crate::media::png::tests::png(5, 4);
    let p = &read(b"image/png", &png).unwrap().artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/png", 5, 4));
    // A progressive grayscale JPEG: one component and two scans.
    let progressive = [
        0xff, 0xd8, 0xff, 0xc2, 0, 11, 8, 0, 2, 0, 3, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0,
        0, 0, 0x12, 0xff, 0xda, 0, 8, 1, 1, 0, 1, 63, 0, 0x34, 0xff, 0xd9,
    ];
    let p = &read(b"image/jpeg", &progressive).unwrap().artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/jpeg", 3, 2));
    // A phone rendition is still held to the one baseline shape.
    assert!(crate::media::jpeg::dimensions(&progressive).is_err());
    // GIF, BMP and TIFF pictures are read, to be sent as JPEGs.
    use crate::media::bitmap::{Format, tests::picture};
    for (mime, format) in [
        (&b"image/gif"[..], Format::Gif),
        (b"image/x-ms-bmp", Format::Bmp),
        (b"image/tiff", Format::Tiff),
    ] {
        let p = &read(mime, &picture(format)).unwrap().artwork[0];
        assert_eq!((p.mime, p.width, p.height), (format.mime(), 2, 1));
    }
    // Valid bytes decide the format when a recognized MIME label is wrong.
    assert_eq!(
        read(b"image/jpeg", &png).unwrap().artwork[0].mime,
        "image/png"
    );
    assert_eq!(
        read(b"image/png", &progressive).unwrap().artwork[0].mime,
        "image/jpeg"
    );
    assert_eq!(
        read(b"image/gif", &png).unwrap().artwork[0].mime,
        "image/png"
    );
    let gif = format!("{:#}", read(b"image/gif", b"GIF89a").unwrap_err());
    assert!(gif.contains("reading a GIF header"), "{gif}");
    let webp = format!("{:#}", read(b"image/webp", &png).unwrap_err());
    assert!(webp.contains("only JPEG, PNG, GIF, BMP and TIFF"), "{webp}");
}
#[test]
fn a_picture_declaring_no_type_is_read_by_its_bytes() {
    let png = crate::media::png::tests::png(5, 4);
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc2, 0, 11, 8, 0, 2, 0, 3, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0,
        0, 0, 0x12, 0xff, 0xda, 0, 8, 1, 1, 0, 1, 63, 0, 0x34, 0xff, 0xd9,
    ];
    // An empty type, or a bare `image/`, declares nothing: the bytes decide.
    for mime in [&b""[..], b"image/", b"IMAGE/"] {
        assert_eq!(image_facts(mime, &png).unwrap(), ("image/png", (5, 4)));
        assert_eq!(image_facts(mime, &jpeg).unwrap(), ("image/jpeg", (3, 2)));
        assert!(image_facts(mime, b"RIFF\x04\0\0\0WEBP").is_err());
        assert!(image_facts(mime, b"not a picture").is_err());
        assert!(image_facts(mime, b"").is_err());
    }
    // Broken taggers sometimes label JPEG bytes as PNG (or the reverse).
    // Decode and validate the actual format without rewriting the source tag.
    assert_eq!(
        image_facts(b"image/jpeg", &png).unwrap(),
        ("image/png", (5, 4))
    );
    assert_eq!(
        image_facts(b"image/png", &jpeg).unwrap(),
        ("image/jpeg", (3, 2))
    );
    assert!(image_facts(b"image/png", b"not a picture").is_err());
    assert!(image_facts(b"image/webp", &png).is_err());
    // An ID3v2.3 APIC frame may leave its type empty.
    let t = Temp::new();
    let apic = [b"\x00".as_slice(), b"\x00\x03\x00", &png].concat();
    let p = &t
        .read(&tagged(vec![frame("APIC", &apic, 3)], 3))
        .unwrap()
        .artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/png", 5, 4));
}
#[test]
fn symlink_source_is_not_read() {
    let t = Temp::new();
    fs::write(t.0.join("target"), audio()).unwrap();
    std::os::unix::fs::symlink(t.0.join("target"), t.0.join("alias.mp3")).unwrap();
    assert!(read(&t.0.join("alias.mp3")).is_err());
}

#[test]
fn legacy_v22_readonly_status_and_empty_frames_preserve_the_original_audio() {
    let temp = Temp::new();
    let mut old = Vec::new();
    for (id, text) in [
        ("TT2", "Legacy title"),
        ("TP1", "Artist"),
        ("TAL", "Album"),
        ("TRK", "3"),
    ] {
        let payload = [b"\0".as_slice(), text.as_bytes()].concat();
        old.extend_from_slice(id.as_bytes());
        old.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        old.extend_from_slice(&payload);
    }
    let bytes = [b"ID3\x02\0\0".as_slice(), &safe(old.len()), &old, &audio()].concat();
    let read = temp.read(&bytes).unwrap();
    assert_eq!(read.metadata.title, ["Legacy title"]);
    assert_eq!(read.metadata.album, ["Album"]);
    assert_eq!(read.metadata.track, Some(3));
    let mut readonly = frame("TIT2", b"\0Legacy title", 3);
    readonly[8] = 0x40;
    let empty = frame("MCDI", b"", 3);
    let newer = temp.read(&tagged(vec![readonly, empty], 3)).unwrap();
    assert_eq!(newer.metadata.title, read.metadata.title);
    assert_eq!(
        newer.fingerprint.audio.sha256,
        read.fingerprint.audio.sha256
    );
}

#[test]
fn incomplete_final_frame_stays_in_the_file_identity_but_not_audio_timing() {
    let temp = Temp::new();
    let complete = [audio(), audio()].concat();
    let original = temp.read(&complete).unwrap();
    let bytes = [complete.as_slice(), &audio()[..200]].concat();
    let partial = temp.read(&bytes).unwrap();
    assert_eq!(partial.fingerprint.file_bytes, bytes.len() as u64);
    assert_eq!(
        partial.fingerprint.audio.sha256,
        original.fingerprint.audio.sha256
    );
    assert_ne!(
        partial.fingerprint.full_file_sha256,
        original.fingerprint.full_file_sha256
    );
    assert_eq!(partial.trailing_tag_bytes, 200);
    partial.transferred_identity().unwrap();
}

#[test]
fn padding_and_tag_stacks_preserve_metadata_artwork_identity_and_source_bytes() {
    let t = Temp::new();
    let original = tagged(vec![frame("TIT2", b"\x03Track", 4)], 4);
    let baseline = t.read(&original).unwrap();
    for padding in [1044, 8193, 17 * 1024 * 1024] {
        let tag_end = original.len() - audio().len();
        let mut bytes = original[..tag_end].to_vec();
        bytes.resize(bytes.len() + padding, 0);
        bytes.extend(audio());
        bytes.extend(b"LYRICSBEGIN000011LYRICS200");
        bytes.extend(crate::media::ape::tests::tag(true, 200));
        let actual = t.read(&bytes).unwrap();
        assert_eq!(actual.metadata.title, baseline.metadata.title);
        assert_eq!(
            actual.fingerprint.audio.sha256,
            baseline.fingerprint.audio.sha256
        );
        assert_eq!(actual.audio_offset, (tag_end + padding) as u64);
        actual.transferred_identity().unwrap();
        assert!(
            read_front_cover(&t.0.join("test.mp3"), &actual)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(t.0.join("test.mp3")).unwrap(), bytes);
        let mut invalid = bytes;
        invalid[tag_end + padding / 2] = 42;
        assert!(t.read(&invalid).is_err());
    }
}

#[test]
fn extended_headers_and_noncanonical_text_encodings_are_read_without_rewriting() {
    let t = Temp::new();
    for version in [3, 4] {
        let ext = if version == 3 {
            vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0]
        } else {
            vec![0, 0, 0, 6, 1, 0]
        };
        let mut body = ext;
        body.extend(frame("TIT2", b"\x03Track", version));
        let mut bytes = b"ID3".to_vec();
        bytes.extend([version, 0, 0x60]);
        bytes.extend(safe(body.len()));
        bytes.extend(body);
        bytes.extend(audio());
        assert_eq!(t.read(&bytes).unwrap().metadata.title, ["Track"]);
    }
    // Extra single NUL is not a truncated audio frame; invalid text units are
    // represented explicitly instead of turning a playable file into an error.
    for (data, expected) in [
        (b"\x01\xff\xfeT\0\0".as_slice(), "T"),
        (b"\x01\xff\xfe\0\xd8".as_slice(), "\u{fffd}"),
        (b"\x03A\xffB".as_slice(), "A\u{fffd}B"),
    ] {
        let bytes = tagged(vec![frame("TIT2", data, 3)], 3);
        assert_eq!(t.read(&bytes).unwrap().metadata.title, [expected]);
    }
}

#[test]
fn missing_mp3_title_projects_filename_without_creating_a_tag_or_changing_bytes() {
    let temp = Temp::new();
    let bytes = tagged(
        vec![frame("TPE1", b"\0Artist", 3), frame("TALB", b"\0Album", 3)],
        3,
    );
    let source = temp.read(&bytes).unwrap();
    assert!(source.metadata.title.is_empty());
    assert_eq!(source.fallback_title.as_deref(), Some("test"));
    assert_eq!(projection::project(&source).unwrap().title, "test");
    let renamed = temp.0.join("Summertime Sadness.mp3");
    fs::rename(temp.0.join("test.mp3"), &renamed).unwrap();
    let renamed_source = read(&renamed).unwrap();
    assert_eq!(
        projection::project(&renamed_source).unwrap().title,
        "Summertime Sadness"
    );
    assert_eq!(renamed_source.fingerprint, source.fingerprint);
    assert_eq!(fs::read(&renamed).unwrap(), bytes);
    assert!(
        read_front_cover(&renamed, &renamed_source)
            .unwrap()
            .is_none()
    );
    let tagged = temp
        .read(&tagged(
            vec![
                frame("TIT2", b"\0Real Title", 3),
                frame("TPE1", b"\0Artist", 3),
                frame("TALB", b"\0Album", 3),
            ],
            3,
        ))
        .unwrap();
    assert!(tagged.fallback_title.is_none());
    assert_eq!(projection::project(&tagged).unwrap().title, "Real Title");
}

#[test]
fn an_untagged_mp3_projects_filename_and_unknown_artist_without_retagging() {
    let temp = Temp::new();
    let bytes = audio();
    let source = temp.read(&bytes).unwrap();
    let projected = projection::project(&source).unwrap();
    assert_eq!(projected.title, "test");
    assert_eq!(projected.artist, "Unknown Artist");
    assert_eq!(projected.album, "Unknown Album");
    assert!(source.metadata.title.is_empty());
    assert!(source.metadata.artist.is_empty());
    assert_eq!(fs::read(temp.0.join("test.mp3")).unwrap(), bytes);
}

#[test]
fn empty_utf16_text_fields_need_no_bom_and_do_not_hide_populated_metadata() {
    let t = Temp::new();
    for version in [3, 4] {
        let bytes = tagged(
            vec![
                frame("TENC", &[1], version),
                frame("TCOP", &[1], version),
                frame("TOPE", &[1], version),
                frame("TCOM", &[1], version),
                frame("TIT2", b"\0Track", version),
                frame("TPE1", b"\0Artist", version),
                frame("TALB", b"\0Album", version),
            ],
            version,
        );
        let source = t.read(&bytes).unwrap();
        assert_eq!(source.metadata.title, ["Track"]);
        assert_eq!(source.metadata.artist, ["Artist"]);
        assert!(
            crate::media::metadata::tag_values(&source.metadata.composer)
                .next()
                .is_none()
        );
        assert_eq!(projection::project(&source).unwrap().album, "Album");
        // Nonempty UTF-16 still needs explicit byte order; never guess it.
        assert!(
            t.read(&tagged(vec![frame("TIT2", b"\x01xx", version)], version))
                .is_err()
        );
    }
}

#[test]
fn every_declared_encoding_accepts_empty_fields_across_legacy_tag_versions() {
    let t = Temp::new();
    for encoding in 0..=3 {
        for version in [3, 4] {
            for id in [
                "TIT2", "TPE1", "TALB", "TPE2", "TCOM", "TCOP", "TYER", "TRCK", "TPOS", "TCMP",
                "TSOT", "TSOP",
            ] {
                let source = t
                    .read(&tagged(vec![frame(id, &[encoding], version)], version))
                    .unwrap();
                assert_eq!(
                    source.fingerprint.audio.frame_count,
                    audio().len() as u32 / 417
                );
            }
        }
        for id in ["TT2", "TP1", "TAL", "TCM", "TCR", "TYE", "TRK", "TPA"] {
            let frame = [id.as_bytes(), &[0, 0, 1], &[encoding]].concat();
            let bytes = [
                b"ID3\x02\0\0".as_slice(),
                &safe(frame.len()),
                &frame,
                &audio(),
            ]
            .concat();
            assert!(t.read(&bytes).is_ok(), "{id} {encoding}");
        }
    }
    let error = t
        .read(&tagged(vec![frame("TIT2", b"\x01bad", 3)], 3))
        .err()
        .unwrap();
    let error = format!("{error:#}");
    assert!(error.contains("TIT2 frame at byte 10"));
    assert!(error.contains("UTF16 BOM"));
    assert!(t.read(&tagged(vec![frame("TIT2", &[255], 3)], 3)).is_err());
}

#[test]
fn mixed_id3_padding_is_accepted_in_tags_and_artwork_rereads_but_not_unknown_bytes() {
    let t = Temp::new();
    let original = tagged(vec![frame("TIT2", b"\0Track", 3)], 3);
    let base = t.read(&original).unwrap();
    for inside in [false, true] {
        let fields = frame("TIT2", b"\0Track", 3);
        let padding = b" \0  \0 ";
        let length = fields.len() + if inside { padding.len() } else { 0 };
        let bytes = [
            b"ID3\x03\0\0".as_slice(),
            &safe(length),
            &fields,
            padding,
            &audio(),
        ]
        .concat();
        let source = t.read(&bytes).unwrap();
        assert_eq!(
            source.fingerprint.audio.sha256,
            base.fingerprint.audio.sha256
        );
        assert_eq!(source.metadata.title, ["Track"]);
        assert!(
            read_front_cover(&t.0.join("test.mp3"), &source)
                .unwrap()
                .is_none()
        );
        let mut bad = bytes.clone();
        bad[10 + fields.len() + 1] = b'X';
        assert!(t.read(&bad).is_err());
    }
}
