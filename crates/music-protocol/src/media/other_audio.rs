//! Non-ALAC classification from actual bytes, never from an extension. MPEG-1
//! Layer III has its own layout record: ID3v2.3/2.4 (optional),
//! consecutive frames, optional ID3v1, every frame header and extent checked.
//! Every other format the phone takes is read by [`crate::media::container`]
//! into its encoded identity. Audio is not decoded.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::Path,
};
const MAX_FILE_BYTES: u64 = crate::protocol::transfer::MAX_AUDIO;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, PartialEq, Eq)]
pub enum OtherCodec {
    /// AAC, HE-AAC, xHE-AAC, FLAC, WAV, AIFF, MPEG-2 and 2.5 Layer III.
    Encoded(crate::media::fingerprint::EncodedAudio),
    Mpeg1Layer3 {
        sample_rate: u32,
        channels: u8,
        frame_count: u32,
        audio_offset: u64,
        audio_bytes: u64,
        trailing_tag_bytes: u32,
    },
}

impl OtherCodec {
    pub fn format(&self) -> crate::media::format::Format {
        match self {
            Self::Encoded(e) => e.format,
            Self::Mpeg1Layer3 { .. } => crate::media::format::Format::Mpeg1Layer3,
        }
    }
}

pub fn read(path: &Path) -> Result<(u64, [u8; 32], OtherCodec)> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.file_type().is_file() && (1..=MAX_FILE_BYTES).contains(&before.len()),
        "other audio must be bounded regular nonsymlink file"
    );
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(same(&before, &opened)?, "other audio changed before open");
    let codec = inspect(&mut file, opened.len())?;
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= opened.len(), "other audio grew during hash");
        digest.update(&buffer[..count]);
    }
    ensure!(
        total == opened.len()
            && same(&opened, &file.metadata()?)?
            && same(&opened, &fs::symlink_metadata(path)?)?,
        "other audio changed during classification"
    );
    Ok((total, digest.finalize().into(), codec))
}
use super::held_source::same_observed_file as same;

pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<OtherCodec> {
    ensure!(
        (8..=MAX_FILE_BYTES).contains(&size),
        "unsupported MPEG input size"
    );
    if size >= 12 {
        if let Some(layout) = crate::media::container::inspect(input, size)? {
            return Ok(OtherCodec::Encoded(layout.encoded));
        }
    }
    input.seek(SeekFrom::Start(0))?;
    let mut first = [0; 10];
    input.read_exact(&mut first)?;
    let mut start = 0u64;
    if &first[..3] == b"ID3" {
        ensure!(
            matches!(first[3], 2 | 3 | 4)
                && first[4] == 0
                && first[6..].iter().all(|b| b & 0x80 == 0),
            "unsupported ID3 header"
        );
        let allowed = match first[3] {
            2 => 0xc0,
            3 => 0xe0,
            _ => 0xf0,
        };
        ensure!(first[5] & !allowed == 0, "reserved ID3 flags");
        let tag_size = first[6..]
            .iter()
            .fold(0u64, |size, byte| (size << 7) | u64::from(*byte));
        ensure!(tag_size <= 16 * 1024 * 1024, "ID3 span exceeds bound");
        start = 10 + tag_size;
        if first[3] == 4 && first[5] & 0x10 != 0 {
            ensure!(start + 10 <= size, "truncated ID3 footer");
            input.seek(SeekFrom::Start(start))?;
            let mut footer = [0; 10];
            input.read_exact(&mut footer)?;
            ensure!(
                &footer[..3] == b"3DI" && footer[3..] == first[3..],
                "invalid ID3 footer"
            );
            start += 10;
        }
    }
    start = crate::media::mpeg_layout::after_id3_padding(input, start, size)?;
    let mut end = crate::media::mpeg_layout::audio_end(input, start, size)?;
    let mut trailing = u32::try_from(size - end)?;
    ensure!(start < end, "MPEG audio span absent");
    let mut offset = start;
    let mut frame_count = 0u32;
    let mut shape = None;
    input.seek(SeekFrom::Start(offset))?;
    let mut body = [0; 1441];
    while offset < end {
        ensure!(
            frame_count < 2_000_000 && end - offset >= 4,
            "MPEG frame count/truncation bound"
        );
        let mut bytes = [0; 4];
        input.read_exact(&mut bytes)?;
        if bytes == [0; 4]
            && frame_count >= 3
            && crate::media::mpeg_layout::after_zeros(input, offset, end)? == end
        {
            trailing += u32::try_from(end - offset)?;
            end = offset;
            break;
        }
        let word = u32::from_be_bytes(bytes);
        ensure!(
            word >> 21 == 0x7ff && (word >> 19) & 3 == 3 && (word >> 17) & 3 == 1,
            "not supported MPEG-1 Layer III frames"
        );
        let bitrate_index = (word >> 12) & 15;
        let rate_index = (word >> 10) & 3;
        ensure!(
            (1..=14).contains(&bitrate_index) && rate_index < 3 && word & 3 != 2,
            "reserved/free MPEG header"
        );
        let rate = [44100, 48000, 32000][rate_index as usize];
        let bitrate = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ][bitrate_index as usize];
        let channels = if (word >> 6) & 3 == 3 { 1 } else { 2 };
        let current = (rate, channels);
        ensure!(
            shape.is_none_or(|old| old == current),
            "MPEG stream changes sample rate/channel count"
        );
        shape = Some(current);
        let length = u64::from(144000 * bitrate / rate + ((word >> 9) & 1));
        ensure!(length >= 4, "invalid MPEG frame length");
        if length > end - offset && frame_count >= 3 {
            // A final incomplete frame cannot produce a complete sample frame.
            // Retain its bytes in full-file identity/transfer, but do not include
            // them in decoded timing or claim that the missing audio is repaired.
            trailing += u32::try_from(end - offset)?;
            end = offset;
            break;
        }
        ensure!(length <= end - offset, "truncated MPEG frame");
        input.read_exact(&mut body[..usize::try_from(length - 4)?])?;
        offset += length;
        frame_count += 1;
    }
    ensure!(
        frame_count >= 2 && offset == end,
        "incomplete MPEG frame sequence"
    );
    let (sample_rate, channels) = shape.context("MPEG stream shape absent")?;
    Ok(OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset: start,
        audio_bytes: end - start,
        trailing_tag_bytes: trailing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn frames() -> Vec<u8> {
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        [frame.clone(), frame].concat()
    }
    #[test]
    fn frame_scan_uses_sequential_reads_not_one_seek_per_frame() {
        struct Counted {
            input: Cursor<Vec<u8>>,
            seeks: usize,
        }
        impl std::io::Read for Counted {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(b)
            }
        }
        impl std::io::Seek for Counted {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.seeks += 1;
                self.input.seek(p)
            }
        }
        let bytes = frames().repeat(2000);
        let size = bytes.len() as u64;
        let mut input = Counted {
            input: Cursor::new(bytes),
            seeks: 0,
        };
        assert!(matches!(
            inspect(&mut input, size).unwrap(),
            OtherCodec::Mpeg1Layer3 {
                frame_count: 4000,
                ..
            }
        ));
        assert!(input.seeks < 50, "{} seeks for 4000 frames", input.seeks);
    }
    #[test]
    fn id3_space_padding_is_not_audio_and_unknown_prefixes_still_fail() {
        let audio = frames().repeat(2);
        for padding in [b"    ".as_slice(), b"\0 \0  \0".as_slice()] {
            let mut bytes = b"ID3\x03\0\0\0\0\0\0".to_vec();
            bytes.extend(padding);
            bytes.extend(&audio);
            let codec = inspect(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert!(
                matches!(codec,OtherCodec::Mpeg1Layer3{frame_count:4,audio_offset,audio_bytes,..} if audio_offset==10+padding.len() as u64 && audio_bytes==audio.len() as u64)
            );
        }
        for prefix in [b"junk".as_slice(), b" \0X".as_slice()] {
            let mut bytes = b"ID3\x03\0\0\0\0\0\0".to_vec();
            bytes.extend(prefix);
            bytes.extend(&audio);
            assert!(inspect(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
        let untagged = [b"    ".as_slice(), audio.as_slice()].concat();
        assert!(inspect(&mut Cursor::new(&untagged), untagged.len() as u64).is_err());
    }

    #[test]
    fn trailing_ape_and_id3v1_preserve_frames_and_transferred_identity() {
        for header in [false, true] {
            for id3v1 in [false, true] {
                let audio = frames().repeat(2);
                let mut bytes = audio.clone();
                bytes.extend(crate::media::ape::tests::tag(header, 70_000));
                if id3v1 {
                    bytes.extend(b"TAG");
                    bytes.resize(bytes.len() + 125, 0);
                }
                let codec = inspect(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
                let OtherCodec::Mpeg1Layer3 {
                    frame_count,
                    audio_offset,
                    audio_bytes,
                    trailing_tag_bytes,
                    sample_rate,
                    channels,
                } = codec
                else {
                    panic!("expected MPEG-1")
                };
                assert_eq!(frame_count, 4);
                assert_eq!(audio_offset, 0);
                assert_eq!(audio_bytes, audio.len() as u64);
                assert_eq!(
                    u64::from(trailing_tag_bytes),
                    bytes.len() as u64 - audio_bytes
                );
                crate::media::fingerprint::TransferredSource::mpeg(
                    bytes.len() as u64,
                    [7; 32],
                    crate::media::fingerprint::EncodedMpeg1Layer3 {
                        sample_rate,
                        channels,
                        frame_count,
                        audio_offset,
                        audio_bytes,
                        trailing_bytes: u64::from(trailing_tag_bytes),
                        frame_bytes_sha256: Sha256::digest(&audio).into(),
                    },
                )
                .unwrap();
                // An arbitrary suffix is still rejected, even after valid tags.
                bytes.push(42);
                assert!(inspect(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
            }
        }
    }

    #[test]
    fn partial_final_frame_is_retained_but_not_counted_as_complete_audio() {
        let complete = frames();
        let mut bytes = [complete.as_slice(), complete.as_slice()].concat();
        bytes.extend_from_slice(&complete[..200]);
        let codec = inspect(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
        assert!(matches!(
            codec,
            OtherCodec::Mpeg1Layer3 {
                frame_count: 4,
                audio_bytes: 1668,
                trailing_tag_bytes: 200,
                ..
            }
        ));
        assert_eq!(bytes.len(), 1868);
    }

    #[test]
    fn classifies_complete_header_sequence_and_explicit_tag_spans() {
        let audio = frames();
        let tagged = [
            b"ID3\x04\x00\x00\x00\x00\x00\x00".as_slice(),
            audio.as_slice(),
        ]
        .concat();
        for (bytes, start) in [(audio, 0), (tagged, 10)] {
            let codec = inspect(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert!(
                codec
                    == OtherCodec::Mpeg1Layer3 {
                        sample_rate: 44100,
                        channels: 2,
                        frame_count: 2,
                        audio_offset: start,
                        audio_bytes: 834,
                        trailing_tag_bytes: 0
                    }
            );
        }
    }
    #[test]
    fn rejects_extension_only_random_truncated_or_mixed_frame_claims() {
        let mut mixed = frames();
        mixed[419] = 0x94;
        let mut reserved = frames();
        reserved[2] = 0;
        let mut tail = frames();
        tail.push(0);
        for bytes in [
            vec![0; 1000],
            frames()[..800].to_vec(),
            mixed,
            reserved,
            tail,
            b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec(),
        ] {
            assert!(inspect(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
    }
}
