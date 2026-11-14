//! Bounded source-side MPEG-1 Layer III and ID3 inspection, never insertion authority.
//! Encoded identity hashes the actual complete frame span, including Xing/LAME
//! frames. Frame samples are a coded duration, not a gapless playback promise.
//! [`read`] takes every other non-ALAC format too: [`audio_source`] fills the
//! same [`Source`] for them, with a stream in place of frame timing.
//!
//! [`audio_source`]: crate::media::audio_source
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub mod gapless;
pub mod projection;
pub mod timing;

// Cached inspection labels are a finite vocabulary; never leak input strings
// to manufacture a static lifetime.
pub(crate) fn cached_word<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<&'static str, D::Error> {
    let word = String::deserialize(d)?;
    for known in [
        "encoded-mpeg1-layer3",
        "encoded-audio",
        "image/jpeg",
        "image/png",
        "image/gif",
        "music-protocol.mpeg-source-timing",
        "LAME",
        "Lavf",
        "Lavc",
        "itunsmpb",
        "lame_tag",
        "smpb_frame",
        "analysis",
    ] {
        if word == known {
            return Ok(known);
        }
    }
    Err(serde::de::Error::custom("unknown cached MPEG label"))
}

pub type InspectionLabel = &'static str;

const MAX_BYTES: u64 = crate::protocol::transfer::MAX_AUDIO;
const MAX_TAG: usize = 16 * 1024 * 1024;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub file_bytes: u64,
    pub full_file_sha256: String,
    pub audio: EncodedMpeg,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedMpeg {
    #[serde(deserialize_with = "crate::media::mpeg::cached_word")]
    pub format: InspectionLabel,
    pub sha256: String,
    pub frame_bytes_sha256: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_count: u32,
    pub audio_bytes: u64,
    pub coded_samples: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tags {
    pub title: Vec<String>,
    pub artist: Vec<String>,
    pub album: Vec<String>,
    pub album_artist: Vec<String>,
    pub genre: Vec<String>,
    pub composer: Vec<String>,
    pub copyright: Vec<String>,
    pub release_date: Vec<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    /// COMM frames that say something. A frame with an empty description and
    /// empty text is left out: it has nothing a catalogue could hold.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    /// Explicit sort names: ID3 TSOT, TSOP, TSOA, TSO2 and TSOC, MP4 sonm,
    /// soar, soal, soaa and soco, and the Vorbis `*SORT` comments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_title: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_artist: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_album: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_album_artist: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_composer: Vec<String>,
    /// What Apple's sync reads from an ID3v2 tag for an MP3's gapless fields.
    #[serde(skip_serializing_if = "gapless::EncoderHints::is_empty")]
    pub encoder_hints: gapless::EncoderHints,
}
impl Tags {
    /// The distinct texts a catalogue comment can come from: comments
    /// without a description. Apple's sync takes an ID3 comment only from a
    /// COMM frame with an empty description, in any language; a described
    /// frame (iTunNORM, iTunSMPB and the like) holds other data.
    pub fn catalogue_comments(&self) -> Vec<&str> {
        let mut texts: Vec<&str> = Vec::new();
        for comment in &self.comments {
            if comment.description.is_empty()
                && !comment.text.is_empty()
                && !texts.contains(&comment.text.as_str())
            {
                texts.push(&comment.text);
            }
        }
        texts
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub language: String,
    pub description: String,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameEvidence {
    pub id: String,
    pub bytes: usize,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picture {
    /// What the bytes are, whatever the container declared.
    #[serde(deserialize_with = "crate::media::mpeg::cached_word")]
    pub mime: InspectionLabel,
    /// The container declared an image type (see [`image_facts`]), as FFmpeg
    /// needs to read a FLAC's picture; not in the JSON.
    #[serde(skip)]
    pub declared_type: bool,
    pub picture_type: u8,
    pub description: String,
    pub bytes: usize,
    pub sha256: String,
    pub width: u16,
    pub height: u16,
    /// The JPEG's markers or the PNG's chunks were checked (not a decode).
    pub structure_verified: bool,
    pub image_decode_verified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Source {
    /// The format's name (`mpeg1-layer3`, `aac-lc`, `flac`, ...).
    pub codec: &'static str,
    pub fingerprint: Fingerprint,
    /// MPEG-1 Layer III frame timing; `None` for every other format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<timing::TimingEvidence>,
    /// Every other format's stream; `None` for MPEG-1 Layer III.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<crate::media::audio_source::Stream>,
    /// The timing and gapless fields Apple's sync sends for an MPEG Layer III
    /// stream; `None` for every other format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apple_timing: Option<gapless::AppleTiming>,
    pub metadata: Tags,
    /// Sync display fallback for an untitled MP3; not an on-disk title tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_title: Option<String>,
    pub id3_version: Option<u8>,
    pub frames: Vec<FrameEvidence>,
    pub artwork: Vec<Picture>,
    pub audio_offset: u64,
    pub trailing_tag_bytes: u32,
}
impl Source {
    pub fn format(&self) -> crate::media::format::Format {
        self.stream
            .as_ref()
            .map_or(crate::media::format::Format::Mpeg1Layer3, |s| s.format)
    }
    /// The frame timing of an MPEG-1 Layer III source.
    pub fn frame_timing(&self) -> Result<&timing::TimingEvidence> {
        self.timing
            .as_ref()
            .context("the source is not MPEG-1 Layer III")
    }
    pub fn transferred_identity(&self) -> Result<crate::media::fingerprint::TransferredSource> {
        fn digest(s: &str) -> Result<[u8; 32]> {
            ensure!(
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid MP3 digest"
            );
            let mut out = [0; 32];
            for (i, b) in out.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
            }
            Ok(out)
        }
        if let Some(stream) = &self.stream {
            ensure!(
                self.codec == stream.format.name()
                    && self.timing.is_none()
                    && self.fingerprint.audio.format == "encoded-audio",
                "audio inspection identity differs"
            );
            let value = crate::media::fingerprint::TransferredSource::encoded(
                self.fingerprint.file_bytes,
                digest(&self.fingerprint.full_file_sha256)?,
                stream.encoded.clone(),
            )?;
            ensure!(
                value.audio.sha256 == digest(&self.fingerprint.audio.sha256)?,
                "audio report and identity differ"
            );
            return Ok(value);
        }
        ensure!(
            self.codec == "mpeg1-layer3"
                && self.fingerprint.audio.format == "encoded-mpeg1-layer3"
                && self.fingerprint.audio.coded_samples
                    == u64::from(self.fingerprint.audio.frame_count) * 1152,
            "MP3 inspection identity differs"
        );
        let a = &self.fingerprint.audio;
        let value = crate::media::fingerprint::TransferredSource::mpeg(
            self.fingerprint.file_bytes,
            digest(&self.fingerprint.full_file_sha256)?,
            crate::media::fingerprint::EncodedMpeg1Layer3 {
                sample_rate: a.sample_rate,
                channels: a.channels,
                frame_count: a.frame_count,
                audio_offset: self.audio_offset,
                audio_bytes: a.audio_bytes,
                trailing_bytes: u64::from(self.trailing_tag_bytes),
                frame_bytes_sha256: digest(&a.frame_bytes_sha256)?,
            },
        )?;
        ensure!(
            value.audio.sha256 == digest(&a.sha256)?,
            "MP3 report/frame identity differs"
        );
        Ok(value)
    }
}
pub fn is_mp3(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("mp3"))
}
/// Whether [`read`] takes `path` by its extension alone: MP3, FLAC, WAV and
/// AIFF. An `.m4a` may be ALAC, which [`read`] refuses.
pub fn is_other_audio(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        ["mp3", "flac", "wav", "wave", "aif", "aiff", "aifc"]
            .iter()
            .any(|e| x.eq_ignore_ascii_case(e))
    })
}
pub fn read(path: &Path) -> Result<Source> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file()
            && !before.file_type().is_symlink()
            && (1..=MAX_BYTES).contains(&before.len()),
        "MP3 source must be a bounded regular nonsymlink file"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        same(&before, &file.metadata()?),
        "MP3 source changed before open"
    );
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, &mut file);
    let mut result = read_file(&mut reader, before.len())?;
    drop(reader);
    if result.stream.is_none()
        && crate::media::metadata::tag_values(&result.metadata.title)
            .next()
            .is_none()
    {
        result.fallback_title = path.file_stem().and_then(|v| v.to_str()).map(str::to_owned);
    }
    ensure!(
        same(&before, &file.metadata()?) && same(&before, &fs::symlink_metadata(path)?),
        "MP3 source changed during inspection"
    );
    Ok(result)
}
use crate::media::held_source::same_file as same;
fn read_file(file: &mut (impl Read + Seek), size: u64) -> Result<Source> {
    if size >= 12 {
        let mut reader = std::io::BufReader::new(&mut *file);
        if let Some(layout) = crate::media::container::inspect(&mut reader, size)? {
            return crate::media::audio_source::read_layout(&mut reader, size, layout);
        }
    }
    let crate::media::other_audio::OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
    } = crate::media::other_audio::inspect(file, size)?
    else {
        anyhow::bail!("not a supported audio format (ALAC goes by its own reader)");
    };
    let frame_hash = hash_span(file, audio_offset, audio_bytes)?;
    let mut encoded = Sha256::new();
    encoded.update(b"music-protocol/encoded-mpeg1-layer3\0");
    encoded.update(sample_rate.to_le_bytes());
    encoded.update([channels]);
    encoded.update(frame_count.to_le_bytes());
    encoded.update(audio_bytes.to_le_bytes());
    encoded.update(frame_hash);
    let timing = timing::inspect(
        file,
        timing::FrameSpan {
            offset: audio_offset,
            bytes: audio_bytes,
            frames: frame_count,
            sample_rate,
            channels,
            sha256: frame_hash,
        },
    )?;
    let full = hash_span(file, 0, size)?;
    // Read only declared metadata into memory; padding is streamed separately.
    let tag_length = crate::media::container::mpeg::id3v2_length(file, size)?.unwrap_or(0);
    ensure!(tag_length <= MAX_TAG as u64 + 20, "ID3 exceeds bound");
    file.rewind()?;
    let mut tag = vec![0; tag_length as usize];
    file.read_exact(&mut tag)?;
    let (metadata, version, frames, artwork) =
        parse_id3(&tag).context("audio frames validated, but ID3 metadata could not be read")?;
    let apple_timing = gapless::analyse(file, audio_offset, audio_bytes, &metadata.encoder_hints)
        .context("Apple's MPEG timing")?;
    // ID3v1 is retained by full-file identity but cannot silently override or
    // fill projected ID3v2 fields. Its presence is explicit; insertion stays shut.
    let source = Source {
        codec: "mpeg1-layer3",
        fingerprint: Fingerprint {
            file_bytes: size,
            full_file_sha256: hex(&full),
            audio: EncodedMpeg {
                format: "encoded-mpeg1-layer3",
                sha256: hex(&encoded.finalize()),
                frame_bytes_sha256: hex(&frame_hash),
                sample_rate,
                channels,
                frame_count,
                audio_bytes,
                coded_samples: u64::from(frame_count) * 1152,
            },
        },
        timing: Some(timing),
        stream: None,
        apple_timing: Some(apple_timing),
        metadata,
        fallback_title: None,
        id3_version: version,
        frames,
        artwork,
        audio_offset,
        trailing_tag_bytes,
    };
    source.transferred_identity()?;
    Ok(source)
}
fn hash_span(file: &mut (impl Read + Seek), start: u64, bytes: u64) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(start))?;
    let mut left = bytes;
    let mut buffer = [0; 65536];
    let mut h = Sha256::new();
    while left > 0 {
        let count = usize::try_from(left.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        h.update(&buffer[..count]);
        left -= count as u64;
    }
    Ok(h.finalize().into())
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn syncsafe(b: &[u8]) -> Result<usize> {
    ensure!(
        b.len() == 4 && b.iter().all(|v| v & 128 == 0),
        "invalid ID3 syncsafe size"
    );
    Ok(b.iter().fold(0, |n, v| (n << 7) | usize::from(*v)))
}
pub(crate) type Parsed = (Tags, Option<u8>, Vec<FrameEvidence>, Vec<Picture>);
pub(crate) fn parse_id3(bytes: &[u8]) -> Result<Parsed> {
    parse_id3_with_picture(bytes, &mut |_, _| Ok(()))
}
pub(crate) fn parse_id3_with_picture(
    bytes: &[u8],
    on_picture: &mut impl FnMut(&Picture, &[u8]) -> Result<()>,
) -> Result<Parsed> {
    let mut tags = Tags::default();
    let mut evidence = Vec::new();
    let mut pictures = Vec::new();
    if bytes.iter().all(|b| *b == 0) {
        return Ok((tags, None, evidence, pictures));
    }
    ensure!(
        bytes.len() >= 10
            && &bytes[..3] == b"ID3"
            && matches!(bytes[3], 2 | 3 | 4)
            && bytes[4] == 0,
        "unsupported ID3 version"
    );
    // Experimental is an advisory flag. Extended headers have explicit sizes.
    // Unsynchronisation/compression still require their own decoding path.
    let allowed = if bytes[3] == 2 {
        0
    } else if bytes[3] == 3 {
        0x60
    } else {
        0x70
    };
    ensure!(
        bytes[5] & !allowed == 0,
        "unsupported ID3 header transformation"
    );
    let version = bytes[3];
    let end = 10 + syncsafe(&bytes[6..10])?;
    ensure!(
        end <= bytes.len()
            && bytes
                .get(end + if bytes[5] & 0x10 != 0 { 10 } else { 0 }..)
                .is_some_and(|rest| rest.iter().all(|b| matches!(*b, 0 | b' '))),
        "ID3 span mismatch"
    );
    let mut offset = 10;
    if bytes[5] & 0x40 != 0 {
        ensure!(end >= 14, "truncated ID3 extended header");
        if version == 3 {
            let length = u32::from_be_bytes(bytes[10..14].try_into()?) as usize;
            ensure!(
                length >= 6 && length <= end - 14,
                "invalid ID3v2.3 extended header size"
            );
            let flags = u16::from_be_bytes(bytes[14..16].try_into()?);
            ensure!(
                flags & !0x8000 == 0 && length == if flags & 0x8000 != 0 { 10 } else { 6 },
                "invalid ID3v2.3 extended header flags"
            );
            offset = 14 + length;
        } else {
            let length = syncsafe(&bytes[10..14])?;
            ensure!(
                length >= 6 && length <= end - 10 && bytes[14] == 1 && bytes[15] & !0x70 == 0,
                "invalid ID3v2.4 extended header"
            );
            let mut at = 16;
            for (flag, count) in [(0x40, 0), (0x20, 5), (0x10, 1)] {
                if bytes[15] & flag != 0 {
                    ensure!(
                        at < 10 + length
                            && bytes[at] == count
                            && at + 1 + count as usize <= 10 + length,
                        "invalid ID3 extended header flag data"
                    );
                    at += 1 + count as usize;
                }
            }
            ensure!(at == 10 + length, "ID3 extended header span mismatch");
            offset = at;
        }
    }
    let mut seen = BTreeMap::<String, usize>::new();
    let mut text_total = 0usize;
    while offset < end {
        if matches!(bytes[offset], 0 | b' ') {
            ensure!(
                bytes[offset..end].iter().all(|b| matches!(*b, 0 | b' ')),
                "invalid bytes in ID3 padding"
            );
            break;
        }
        let header = if version == 2 { 6 } else { 10 };
        let id_bytes = if version == 2 { 3 } else { 4 };
        ensure!(
            evidence.len() < 4096 && end - offset >= header,
            "ID3 frame count/header bound"
        );
        let id = &bytes[offset..offset + id_bytes];
        ensure!(
            id.iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "invalid ID3 frame ID"
        );
        let id = std::str::from_utf8(id)?.to_owned();
        let len = if version == 2 {
            bytes[offset + 3..offset + 6]
                .iter()
                .fold(0usize, |n, b| (n << 8) | usize::from(*b))
        } else if version == 4 {
            syncsafe(&bytes[offset + 4..offset + 8])?
        } else {
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize
        };
        ensure!(len <= end - offset - header, "invalid ID3 frame extent");
        if version != 2 {
            let status_mask = if version == 3 { 0xe0 } else { 0x70 };
            ensure!(
                bytes[offset + 8] & !status_mask == 0 && bytes[offset + 9] == 0,
                "unsupported ID3 frame flags"
            );
        }
        let raw_data = &bytes[offset + header..offset + header + len];
        if len == 0 {
            offset += header;
            continue;
        }
        let mapped = if version == 2 {
            lofty::id3::v2::upgrade_v2(&id).unwrap_or(id.as_str())
        } else {
            &id
        };
        let legacy_picture;
        let data = if version == 2 && mapped == "APIC" {
            ensure!(raw_data.len() >= 5, "truncated ID3v2.2 picture");
            let mime: &[u8] = match &raw_data[1..4] {
                b"JPG" => b"image/jpeg",
                b"PNG" => b"image/png",
                b"GIF" => b"image/gif",
                _ => anyhow::bail!("unsupported ID3v2.2 picture format"),
            };
            legacy_picture = [&raw_data[..1], mime, b"\0", &raw_data[4..]].concat();
            legacy_picture.as_slice()
        } else {
            raw_data
        };
        evidence.push(FrameEvidence {
            id: id.clone(),
            bytes: len,
            sha256: hex(&Sha256::digest(raw_data)),
        });
        *seen.entry(mapped.to_owned()).or_default() += 1;
        let target = match mapped {
            "TIT2" => Some(&mut tags.title),
            "TPE1" => Some(&mut tags.artist),
            "TALB" => Some(&mut tags.album),
            "TPE2" => Some(&mut tags.album_artist),
            "TCON" => Some(&mut tags.genre),
            "TCOM" => Some(&mut tags.composer),
            "TCOP" => Some(&mut tags.copyright),
            "TDRC" | "TYER" => Some(&mut tags.release_date),
            "TSOT" => Some(&mut tags.sort_title),
            "TSOP" => Some(&mut tags.sort_artist),
            "TSOA" => Some(&mut tags.sort_album),
            "TSO2" => Some(&mut tags.sort_album_artist),
            "TSOC" => Some(&mut tags.sort_composer),
            _ => None,
        };
        if let Some(target) = target {
            let values = text_frame(data, version)
                .with_context(|| format!("reading ID3 {id} frame at byte {offset}"))?;
            text_total += values.iter().map(String::len).sum::<usize>();
            target.extend(values);
            ensure!(
                target.len() <= 32 && text_total <= MAX_TEXT,
                "ID3 text exceeds bound"
            );
        } else if matches!(mapped, "TRCK" | "TPOS" | "TCMP") {
            ensure!(seen[mapped] == 1, "duplicate ID3 numeric frame");
            let values = text_frame(data, version)
                .with_context(|| format!("reading ID3 {id} frame at byte {offset}"))?;
            // An empty number is no number, as an empty text is no text.
            let values: Vec<_> = crate::media::metadata::tag_values(&values).collect();
            ensure!(values.len() <= 1, "ambiguous ID3 numeric frame");
            if let Some(&value) = values.first() {
                if mapped == "TCMP" {
                    tags.compilation = Some(match value {
                        "0" => false,
                        "1" => true,
                        _ => anyhow::bail!("invalid ID3 compilation"),
                    });
                } else {
                    let (number, total) = number_pair(value)?;
                    if mapped == "TRCK" {
                        tags.track = Some(number);
                        tags.track_total = total;
                    } else {
                        tags.disc = Some(number);
                        tags.disc_total = total;
                    }
                }
            }
        } else if matches!(mapped, "TENC" | "TSSE") {
            // Only Apple's gapless rule reads these; one it cannot decode
            // names no encoder.
            if let Some(text) = text_frame(data, version)
                .ok()
                .and_then(|v| v.into_iter().next())
            {
                tags.encoder_hints.encoder(&text);
            }
        } else if mapped == "COMM" {
            if let Some(comment) = comment_frame(data, version)
                .with_context(|| format!("reading ID3 {id} frame at byte {offset}"))?
            {
                tags.encoder_hints
                    .comment(&comment.description, &comment.text);
                text_total += comment.description.len() + comment.text.len();
                tags.comments.push(comment);
                ensure!(
                    tags.comments.len() <= 32 && text_total <= MAX_TEXT,
                    "ID3 comments exceed bound"
                );
            }
        } else if mapped == "APIC" {
            ensure!(pictures.len() < 8, "too many ID3 pictures");
            let (facts, image) = picture_payload(data, version)
                .with_context(|| format!("reading ID3 {id} frame at byte {offset}"))?;
            on_picture(&facts, image)?;
            pictures.push(facts);
        }
        offset += header + len;
    }
    Ok((tags, Some(version), evidence, pictures))
}
/// Encoding, three-byte language, a terminated description, then the text.
/// None when both description and text are empty.
fn comment_frame(data: &[u8], version: u8) -> Result<Option<Comment>> {
    ensure!(
        data.len() >= 4 && data.len() <= MAX_TEXT,
        "ID3 comment size bound"
    );
    let (encoding, language, rest) = (data[0], &data[1..4], &data[4..]);
    ensure!(
        language.iter().all(|b| b.is_ascii_alphabetic()) || language == [0, 0, 0],
        "invalid ID3 comment language"
    );
    let width = if matches!(encoding, 1 | 2) { 2 } else { 1 };
    let terminator = rest
        .chunks_exact(width)
        .position(|c| c.iter().all(|b| *b == 0))
        .context("ID3 comment description terminator missing")?
        * width;
    let decode = |bytes: &[u8]| -> Result<String> {
        if bytes.is_empty() {
            ensure!(encoding <= 3, "invalid ID3 comment encoding");
            return Ok(String::new());
        }
        decode_text(encoding, bytes, version)
    };
    let description = decode(&rest[..terminator])?;
    let text = decode(&rest[terminator + width..])?
        .trim_end_matches('\0')
        .to_owned();
    ensure!(
        !description.contains('\0') && !text.contains('\0'),
        "ambiguous ID3 comment text"
    );
    if description.is_empty() && text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Comment {
        language: language.iter().map(|b| char::from(*b)).collect(),
        description,
        text,
    }))
}
pub(crate) fn number_pair(s: &str) -> Result<(u32, Option<u32>)> {
    let mut p = s.split('/');
    let number = p.next().context("empty numeric tag")?.parse()?;
    let total = p.next().map(str::parse).transpose()?;
    ensure!(p.next().is_none(), "invalid numeric pair");
    Ok((number, total))
}
fn text_frame(data: &[u8], version: u8) -> Result<Vec<String>> {
    ensure!(
        !data.is_empty() && data.len() <= MAX_TEXT,
        "ID3 text size bound"
    );
    let text = decode_text(data[0], &data[1..], version)?;
    let values: Vec<_> = text
        .trim_end_matches('\0')
        .split('\0')
        .map(str::to_owned)
        .collect();
    ensure!(values.len() <= 32, "ID3 text value bound");
    Ok(values)
}
fn decode_text(encoding: u8, data: &[u8], _version: u8) -> Result<String> {
    // Cleared fields can contain only the encoding byte. No text means there
    // is no byte order to infer, even if the writer omitted a UTF-16 BOM.
    if data.is_empty() {
        ensure!(encoding <= 3, "unknown ID3 text encoding");
        return Ok(String::new());
    }
    // Read the declared encoding even when a writer put UTF-8 in a v2.3 tag.
    // Replacement characters describe malformed text; the source stays intact.
    let s = match encoding {
        0 => data.iter().map(|v| char::from(*v)).collect(),
        3 => String::from_utf8_lossy(data).into_owned(),
        1 | 2 => {
            let (le, raw) = if encoding == 1 {
                ensure!(data.len() >= 2, "missing UTF16 BOM");
                match &data[..2] {
                    [0xff, 0xfe] => (true, &data[2..]),
                    [0xfe, 0xff] => (false, &data[2..]),
                    _ => anyhow::bail!("invalid UTF16 BOM"),
                }
            } else {
                (false, data)
            };
            let words: Vec<u16> = raw
                .chunks_exact(2)
                .map(|b| {
                    if le {
                        u16::from_le_bytes([b[0], b[1]])
                    } else {
                        u16::from_be_bytes([b[0], b[1]])
                    }
                })
                .collect();
            let mut text = String::from_utf16_lossy(&words);
            if raw.len() % 2 != 0 && raw.last() != Some(&0) {
                text.push(char::REPLACEMENT_CHARACTER);
            }
            text
        }
        _ => anyhow::bail!("unknown ID3 text encoding"),
    };
    ensure!(
        s.len() <= MAX_TEXT
            && !s.chars().any(|c| c.is_control()
                && c != '\0'
                && c != '\t'
                && c != '\n'
                && c != '\r'),
        "invalid or overlong ID3 text"
    );
    Ok(s)
}
fn picture_payload(data: &[u8], version: u8) -> Result<(Picture, &[u8])> {
    ensure!(data.len() > 5, "truncated APIC");
    let encoding = data[0];
    let end = data[1..]
        .iter()
        .position(|b| *b == 0)
        .context("APIC MIME terminator missing")?
        + 1;
    let mime = &data[1..end];
    let kind = *data.get(end + 1).context("APIC type missing")?;
    ensure!(kind <= 20, "invalid APIC type");
    let rest = &data[end + 2..];
    let width = if encoding == 1 || encoding == 2 { 2 } else { 1 };
    let terminator = rest
        .chunks_exact(width)
        .position(|c| c.iter().all(|b| *b == 0))
        .context("APIC description terminator missing")?
        * width;
    let description = if terminator == 0 {
        ensure!(encoding <= 3, "invalid APIC encoding");
        String::new()
    } else {
        decode_text(encoding, &rest[..terminator], version)?
    };
    let image = &rest[terminator + width..];
    let (held, (w, h)) = image_facts(mime, image)?;
    Ok((
        Picture {
            mime: held,
            declared_type: declares_type(mime),
            picture_type: kind,
            description,
            bytes: image.len(),
            sha256: hex(&Sha256::digest(image)),
            width: w,
            height: h,
            structure_verified: true,
            image_decode_verified: false,
        },
        image,
    ))
}

/// Whether a declared picture type names a type, rather than being empty or a
/// bare `image/`.
pub(crate) fn declares_type(mime: &[u8]) -> bool {
    !matches!(mime.to_ascii_lowercase().as_slice(), b"" | b"image/")
}

/// What a picture's bytes are: a JPEG of any kind or a PNG, the two cover types
/// Apple's sync sends unchanged, or a GIF, BMP or TIFF, which it converts (and
/// which sync sends as a JPEG, [`bitmap::to_jpeg`]). The validated bytes take
/// precedence over a recognized but incorrectly declared MIME type. An empty type, or a bare `image/`, declares nothing (ID3v2.3
/// reads an omitted one as `image/`, and taggers leave it empty in FLAC): the
/// bytes alone decide, and still have to be one of those types.
///
/// [`bitmap::to_jpeg`]: crate::media::bitmap::to_jpeg
pub(crate) fn image_facts(mime: &[u8], image: &[u8]) -> Result<(&'static str, (u16, u16))> {
    use crate::media::bitmap::Format;
    match mime.to_ascii_lowercase().as_slice() {
        _ if !declares_type(mime) => None,
        b"image/jpeg" | b"image/jpg" => Some("image/jpeg"),
        b"image/png" => Some("image/png"),
        b"image/gif" => Some("image/gif"),
        b"image/bmp" | b"image/x-bmp" | b"image/x-ms-bmp" => Some("image/bmp"),
        b"image/tiff" => Some("image/tiff"),
        _ => anyhow::bail!(
            "only JPEG, PNG, GIF, BMP and TIFF pictures are read, not {:?}",
            String::from_utf8_lossy(mime)
        ),
    };
    let held = if image.starts_with(&crate::media::png::SIGNATURE) {
        "image/png"
    } else {
        Format::of(image).map_or("image/jpeg", Format::mime)
    };
    Ok((
        held,
        match held {
            "image/png" => crate::media::png::dimensions(image)?,
            "image/jpeg" => crate::media::jpeg::source_dimensions(image)?,
            _ => crate::media::bitmap::dimensions(
                image,
                Format::of(image).context("picture format")?,
            )?,
        },
    ))
}

/// Which of a source's pictures sync sends as its cover: the one picture
/// marked as the front cover (type 3; an MP4 `covr` image counts as one), or
/// the file's only picture whatever its type (FFmpeg marks an attached
/// picture "other" by default). A file with several pictures and not exactly
/// one front cover goes without a cover; [`cover_left_out`] says why. Every
/// picture stays in the transferred file either way.
pub fn cover_index(pictures: &[Picture]) -> Option<usize> {
    if pictures.len() == 1 {
        return Some(0);
    }
    let mut fronts = pictures
        .iter()
        .enumerate()
        .filter(|(_, p)| p.picture_type == 3)
        .map(|(index, _)| index);
    match (fronts.next(), fronts.next()) {
        (Some(index), None) => Some(index),
        _ => None,
    }
}

/// The picture [`cover_index`] chooses.
pub fn cover_picture(pictures: &[Picture]) -> Option<&Picture> {
    cover_index(pictures).map(|index| &pictures[index])
}

/// Why a source that holds pictures goes without a cover; `None` when it
/// has its cover or holds no picture.
pub fn cover_left_out(pictures: &[Picture]) -> Option<String> {
    if pictures.is_empty() || cover_index(pictures).is_some() {
        return None;
    }
    let fronts = pictures.iter().filter(|p| p.picture_type == 3).count();
    Some(if fronts == 0 {
        format!(
            "it holds {} pictures and none is marked as the front cover, so it goes without a cover",
            pictures.len()
        )
    } else {
        format!("it holds {fronts} pictures marked as the front cover, so it goes without a cover")
    })
}

/// Exact cover bytes (the picture [`cover_index`] chooses, as the file holds
/// it) without assigning an MP4 checksum or a native artwork permit. Every
/// other source picture remains embedded and hash-bound in the unchanged
/// transferred file.
pub struct EmbeddedCover {
    pub(crate) bytes: Vec<u8>,
    pub(crate) facts: Picture,
}
impl EmbeddedCover {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn facts(&self) -> &Picture {
        &self.facts
    }
}
pub fn read_front_cover(path: &Path, expected: &Source) -> Result<Option<EmbeddedCover>> {
    let identity = expected.transferred_identity()?;
    let mut held = crate::media::held_source::BoundSource::open_observed(
        path,
        identity.file_bytes,
        identity.full_file_sha256,
    )?;
    let bytes = held.read_for_retention()?;
    if expected.stream.is_some() {
        return crate::media::audio_source::front_cover(&bytes, expected);
    }
    let tag = bytes
        .get(..usize::try_from(expected.audio_offset)?)
        .context("MP3 tag span differs")?;
    cover_from_tag(tag, expected)
}

/// Extract and validate only ID3 picture/tag bytes. The caller must bind the
/// whole audio file separately at upload; this does not verify its audio bytes.
pub fn read_front_cover_for_upload(
    path: &Path,
    expected: &Source,
) -> Result<Option<EmbeddedCover>> {
    if expected.stream.is_some() {
        return read_front_cover(path, expected);
    }
    if cover_index(&expected.artwork).is_none() {
        return Ok(None);
    }
    let identity = expected.transferred_identity()?;
    let mut held =
        crate::media::held_source::BoundSource::open_for_upload(path, identity.file_bytes)?;
    let length =
        crate::media::container::mpeg::id3v2_length(held.file(), identity.file_bytes)?.unwrap_or(0);
    ensure!(
        length <= MAX_TAG as u64 + 20,
        "ID3 picture prefix exceeds bound"
    );
    held.file().rewind()?;
    let mut tag = vec![0; length as usize];
    held.file().read_exact(&mut tag)?;
    held.check()?;
    cover_from_tag(&tag, expected)
}

fn cover_from_tag(tag: &[u8], expected: &Source) -> Result<Option<EmbeddedCover>> {
    let chosen = cover_index(&expected.artwork);
    let (mut front, mut at) = (None, 0);
    let (tags, version, frames, pictures) = parse_id3_with_picture(tag, &mut |facts, image| {
        if Some(at) == chosen {
            front = Some(EmbeddedCover {
                bytes: image.to_vec(),
                facts: facts.clone(),
            });
        }
        at += 1;
        Ok(())
    })?;
    ensure!(
        tags == expected.metadata
            && version == expected.id3_version
            && frames == expected.frames
            && pictures == expected.artwork,
        "MP3 artwork/source inspection differs"
    );
    ensure!(
        chosen.is_none() || front.is_some(),
        "MP3 cover picture is absent"
    );
    Ok(front)
}
#[cfg(test)]
mod tests;

#[cfg(test)]
use crate::test_support::mpeg as test_fixture;
