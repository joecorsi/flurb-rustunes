//! Metadata boundaries shared by every MPEG Layer III reader. These helpers
//! never find audio by skipping arbitrary junk or searching inside tag values.
use anyhow::{Context, Result, ensure};
use std::io::{Read, Seek, SeekFrom};

pub(crate) const MAX_PREFIX: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_TRAILING: u64 = 16 * 1024 * 1024;

/// Skip only zero bytes, in fixed-size chunks, up to the actual file boundary.
/// The caller must still validate the complete, consecutive audio frames.
pub(crate) fn after_zeros(input: &mut (impl Read + Seek), start: u64, end: u64) -> Result<u64> {
    after_padding(input, start, end, false)
}

/// Accept zero/ASCII-space padding after a declared ID3 tag. Never search
/// through arbitrary junk; callers validate all consecutive audio frames.
pub(crate) fn after_id3_padding(
    input: &mut (impl Read + Seek),
    start: u64,
    end: u64,
) -> Result<u64> {
    after_padding(input, start, end, start > 0)
}

fn after_padding(
    input: &mut (impl Read + Seek),
    mut start: u64,
    end: u64,
    spaces: bool,
) -> Result<u64> {
    let mut buffer = [0; 8192];
    while start < end {
        let length = (end - start).min(buffer.len() as u64) as usize;
        input.seek(SeekFrom::Start(start))?;
        input.read_exact(&mut buffer[..length])?;
        if let Some(at) = buffer[..length]
            .iter()
            .position(|b| *b != 0 && !(spaces && *b == b' '))
        {
            return Ok(start + at as u64);
        }
        start += length as u64;
    }
    Ok(start)
}

fn bytes_at(input: &mut (impl Read + Seek), at: u64, count: usize) -> Result<Vec<u8>> {
    input.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0; count];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Peel identified trailing metadata containers, including stacked tags.
/// Return the audio end; all excluded bytes remain in full-file hashes/copies.
pub(crate) fn audio_end(input: &mut (impl Read + Seek), start: u64, size: u64) -> Result<u64> {
    let mut end = size;
    for _ in 0..32 {
        let previous = end;
        if end.saturating_sub(start) >= 128 && bytes_at(input, end - 128, 3)? == b"TAG" {
            end -= 128;
            if end.saturating_sub(start) >= 227 && bytes_at(input, end - 227, 4)? == b"TAG+" {
                end -= 227;
            }
        } else {
            let ape = crate::media::ape::audio_end(input, start, end)?;
            if ape != end {
                end = ape;
            } else if end.saturating_sub(start) >= 15
                && bytes_at(input, end - 9, 9)? == b"LYRICS200"
            {
                let length = bytes_at(input, end - 15, 6)?;
                ensure!(
                    length.iter().all(u8::is_ascii_digit),
                    "invalid Lyrics3 size"
                );
                let length = std::str::from_utf8(&length)?.parse::<u64>()?;
                let next = end
                    .checked_sub(length + 15)
                    .filter(|v| *v >= start)
                    .context("Lyrics3 exceeds file")?;
                ensure!(
                    length >= 11 && bytes_at(input, next, 11)? == b"LYRICSBEGIN",
                    "invalid Lyrics3 boundary"
                );
                end = next;
            } else if end.saturating_sub(start) >= 20 && bytes_at(input, end - 10, 3)? == b"3DI" {
                let footer = bytes_at(input, end - 10, 10)?;
                ensure!(
                    footer[3] == 4
                        && footer[4] == 0
                        && footer[5] & 0x10 != 0
                        && footer[6..].iter().all(|b| b & 128 == 0),
                    "invalid appended ID3 footer"
                );
                let length = footer[6..]
                    .iter()
                    .fold(0u64, |v, b| (v << 7) | u64::from(*b));
                let next = end
                    .checked_sub(length + 20)
                    .filter(|v| *v >= start)
                    .context("appended ID3 exceeds file")?;
                let header = bytes_at(input, next, 10)?;
                ensure!(
                    &header[..3] == b"ID3" && header[3..] == footer[3..],
                    "appended ID3 header/footer differ"
                );
                end = next;
            } else if end.saturating_sub(start) >= 20
                && bytes_at(input, end - 9, 9)? == b"LYRICSEND"
            {
                let lower = start.max(end.saturating_sub(5120));
                let bytes = bytes_at(input, lower, (end - lower) as usize)?;
                let at = bytes
                    .windows(11)
                    .rposition(|b| b == b"LYRICSBEGIN")
                    .context("Lyrics3v1 start absent")?;
                end = lower + at as u64;
            }
        }
        ensure!(
            size - end <= MAX_TRAILING,
            "trailing MPEG metadata exceeds bound"
        );
        if previous == end {
            return Ok(end);
        }
    }
    anyhow::bail!("too many trailing MPEG tags")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn id3_padding_is_streamed_across_chunks_without_scanning_through_unknown_bytes() {
        for count in [0, 4, 8193, 100_000] {
            let mut bytes = vec![99; 10];
            bytes.extend((0..count).map(|i| if i % 2 == 0 { 0 } else { b' ' }));
            bytes.extend(b"\xff\xfb\x90\0");
            assert_eq!(
                after_id3_padding(&mut Cursor::new(&bytes), 10, bytes.len() as u64).unwrap(),
                10 + count as u64
            );
        }
        let bytes = b"0123456789 \0junk\xff\xfb\x90\0";
        assert_eq!(
            after_id3_padding(&mut Cursor::new(bytes), 10, bytes.len() as u64).unwrap(),
            12
        );
        assert_eq!(after_zeros(&mut Cursor::new(b" \0"), 0, 2).unwrap(), 0);
    }

    #[test]
    fn zeros_are_streamed_but_unknown_prefix_bytes_are_never_skipped() {
        for n in [0, 1044, 8193, 100_000] {
            let mut bytes = vec![0; n];
            bytes.extend(b"\xff\xfb\x90\0");
            assert_eq!(
                after_zeros(&mut Cursor::new(&bytes), 0, bytes.len() as u64).unwrap(),
                n as u64
            );
        }
        assert_eq!(
            after_zeros(&mut Cursor::new(b"junk\0\xff"), 0, 6).unwrap(),
            0
        );
    }
    #[test]
    fn stacked_lyrics_ape_enhanced_id3_and_appended_id3_have_exact_boundaries() {
        let mut bytes = vec![42; 100];
        bytes.extend(b"LYRICSBEGINhelloLYRICSEND");
        bytes.extend(b"LYRICSBEGIN000011LYRICS200");
        bytes.extend(crate::media::ape::tests::tag(true, 5));
        bytes.extend(b"TAG+");
        bytes.resize(bytes.len() + 223, 0);
        bytes.extend(b"TAG");
        bytes.resize(bytes.len() + 125, 0);
        bytes.extend(b"ID3\x04\0\x10\0\0\0\0");
        bytes.extend(b"3DI\x04\0\x10\0\0\0\0");
        let end = bytes.len() as u64;
        assert_eq!(audio_end(&mut Cursor::new(&bytes), 0, end).unwrap(), 100);
        let last = bytes.len() - 1;
        bytes[last] = 127;
        assert!(audio_end(&mut Cursor::new(&bytes), 0, end).is_err());
    }
}
