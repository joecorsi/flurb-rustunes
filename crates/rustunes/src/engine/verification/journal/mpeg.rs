//! Actual-byte MPEG verification only. No metadata projection, native timing,
//! journal transition or write capability is produced here.
use super::ReadControl;
use crate::engine::state::audio_index::{OtherCodec, inspect_mpeg_frames};
use anyhow::{Result, ensure};
use music_protocol::media::fingerprint::{EncodedMpeg1Layer3, TransferredSource};
use music_protocol::protocol::transfer::{MAX_AUDIO, hex};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::io::Cursor;
use std::io::{Read, Seek, SeekFrom};

/// Present only for an MPEG binding; ALAC reports omit the entire field.
/// Facts remain provisional until the enclosing batch's final state guard.
#[derive(Serialize)]
pub struct MpegFrameReport {
    format: &'static str,
    sample_rate: u32,
    channels: u8,
    frame_count: u32,
    audio_offset: u64,
    audio_bytes: u64,
    trailing_tag_bytes: u32,
    frame_bytes_sha256: String,
    encoded_audio_sha256: String,
    pub(super) encoded_identity_verified: bool,
}
impl MpegFrameReport {
    pub(super) fn matches(&self, expected: &TransferredSource) -> bool {
        expected.audio.mpeg().is_some_and(|e| {
            self.sample_rate == e.sample_rate
                && self.channels == e.channels
                && self.frame_count == e.frame_count
                && self.audio_offset == e.audio_offset
                && self.audio_bytes == e.audio_bytes
                && u64::from(self.trailing_tag_bytes) == e.trailing_bytes
                && self.frame_bytes_sha256 == hex(&e.frame_bytes_sha256)
                && self.encoded_audio_sha256 == hex(&expected.audio.sha256)
        })
    }
}

struct Controlled<'a, R> {
    bytes: R,
    control: &'a ReadControl,
}
impl<R: Read> Read for Controlled<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.control.check().map_err(std::io::Error::other)?;
        self.bytes.read(output)
    }
}
impl<R: Seek> Seek for Controlled<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.control.check().map_err(std::io::Error::other)?;
        self.bytes.seek(position)
    }
}
fn hash_reader(input: &mut impl Read, size: u64, control: &ReadControl) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        control.check()?;
        let n = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..n])?;
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(hash.finalize().into())
}
#[cfg(test)]
pub(super) fn inspect(bytes: &[u8], control: &ReadControl) -> Result<([u8; 32], MpegFrameReport)> {
    inspect_reader(&mut Cursor::new(bytes), bytes.len() as u64, control)
}
pub(super) fn inspect_reader(
    input: &mut (impl Read + Seek),
    size: u64,
    control: &ReadControl,
) -> Result<([u8; 32], MpegFrameReport)> {
    control.check()?;
    ensure!(size > 0 && size <= MAX_AUDIO, "invalid MPEG readback size");
    input.rewind()?;
    let codec = inspect_mpeg_frames(
        &mut Controlled {
            bytes: &mut *input,
            control,
        },
        size,
    )?;
    let OtherCodec::Mpeg1Layer3 {
        audio_offset,
        audio_bytes,
        ..
    } = &codec
    else {
        anyhow::bail!("readback is not an MPEG-1 Layer III stream");
    };
    ensure!(
        audio_offset
            .checked_add(*audio_bytes)
            .is_some_and(|end| end <= size),
        "MPEG span outside file"
    );
    input.seek(SeekFrom::Start(*audio_offset))?;
    let frame_hash = hash_reader(input, *audio_bytes, control)?;
    input.rewind()?;
    let file_hash = hash_reader(input, size, control)?;
    control.check()?;
    Ok((file_hash, report(size, file_hash, &codec, frame_hash)?))
}

/// Frame facts measured on actual bytes: by this readback, or by an earlier
/// verified copy or upload of the same unchanged file. Never from the binding.
pub(super) fn report(
    file_bytes: u64,
    full_file_sha256: [u8; 32],
    codec: &OtherCodec,
    frame_bytes_sha256: [u8; 32],
) -> Result<MpegFrameReport> {
    let OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
    } = *codec
    else {
        anyhow::bail!("frame facts are for an MPEG-1 Layer III stream");
    };
    let source = TransferredSource::mpeg(
        file_bytes,
        full_file_sha256,
        EncodedMpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_bytes: u64::from(trailing_tag_bytes),
            frame_bytes_sha256,
        },
    )?;
    Ok(MpegFrameReport {
        format: "encoded-mpeg1-layer3",
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
        frame_bytes_sha256: hex(&frame_bytes_sha256),
        encoded_audio_sha256: hex(&source.audio.sha256),
        encoded_identity_verified: false,
    })
}
