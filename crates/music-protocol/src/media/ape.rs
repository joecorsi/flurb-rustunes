//! Bounded trailing APE tag framing. Values remain opaque; they are not audio.
//! Layout reference: TagLib's ape/ape-tag-format.txt and apefooter.cpp.
use anyhow::{Context, Result, ensure};
use std::io::{Read, Seek, SeekFrom};

pub(crate) const MAX_TAG_BYTES: u64 = 16 * 1024 * 1024;

/// `end` is already before any ID3v1 tag. Return the start of a validated APE
/// region, or `end` when no APE footer exists. Never hunt through audio bytes.
pub(crate) fn audio_end(input: &mut (impl Read + Seek), start: u64, end: u64) -> Result<u64> {
    if end.saturating_sub(start) < 32 {
        return Ok(end);
    }
    input.seek(SeekFrom::Start(end - 32))?;
    let mut footer = [0; 32];
    input.read_exact(&mut footer)?;
    if &footer[..8] != b"APETAGEX" {
        return Ok(end);
    }
    let number = |at| u32::from_le_bytes(footer[at..at + 4].try_into().unwrap());
    let version = number(8);
    let size = u64::from(number(12));
    let count = number(16);
    let flags = number(20);
    ensure!(
        matches!(version, 1000 | 2000),
        "unsupported APE tag version"
    );
    ensure!(
        footer[24..] == [0; 8] && flags & 0x2000_0000 == 0,
        "invalid APE footer"
    );
    let header_bytes = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
    let total = size
        .checked_add(header_bytes)
        .context("APE size overflow")?;
    ensure!(
        (32..=MAX_TAG_BYTES).contains(&total) && size >= 32 && count <= 50_000,
        "APE tag exceeds bounds"
    );
    let tag_start = end
        .checked_sub(total)
        .filter(|v| *v >= start)
        .context("APE tag overlaps audio start")?;
    input.seek(SeekFrom::Start(tag_start))?;
    if header_bytes != 0 {
        let mut header = [0; 32];
        input.read_exact(&mut header)?;
        let mut expected = footer;
        expected[20..24].copy_from_slice(&(flags | 0x2000_0000).to_le_bytes());
        ensure!(header == expected, "APE header/footer differ");
    }
    let mut items = vec![0; (size - 32) as usize];
    input.read_exact(&mut items)?;
    let mut at = 0usize;
    for _ in 0..count {
        let head = items.get(at..at + 8).context("truncated APE item")?;
        let bytes = u32::from_le_bytes(head[..4].try_into()?) as usize;
        let item_flags = u32::from_le_bytes(head[4..].try_into()?);
        ensure!(
            item_flags & !7 == 0 && (item_flags >> 1) & 3 != 3,
            "reserved APE item flags"
        );
        at += 8;
        let rest = &items[at..];
        let key_size = rest
            .iter()
            .position(|b| *b == 0)
            .context("unterminated APE key")?;
        ensure!(
            (2..=255).contains(&key_size)
                && rest[..key_size].iter().all(|b| (32..=126).contains(b)),
            "invalid APE key"
        );
        at = at
            .checked_add(key_size + 1)
            .and_then(|v| v.checked_add(bytes))
            .filter(|v| *v <= items.len())
            .context("APE value exceeds tag")?;
    }
    ensure!(at == items.len(), "APE item count/size differ");
    Ok(tag_start)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;
    pub(crate) fn tag(header: bool, payload: usize) -> Vec<u8> {
        let mut items = (payload as u32).to_le_bytes().to_vec();
        items.extend(0u32.to_le_bytes());
        items.extend(b"TITLE\0");
        items.resize(items.len() + payload, b'x');
        let mut footer = b"APETAGEX".to_vec();
        footer.extend(2000u32.to_le_bytes());
        footer.extend(((items.len() + 32) as u32).to_le_bytes());
        footer.extend(1u32.to_le_bytes());
        footer.extend((if header { 0x8000_0000u32 } else { 0 }).to_le_bytes());
        footer.extend([0; 8]);
        let mut out = Vec::new();
        if header {
            let mut h = footer.clone();
            h[20..24].copy_from_slice(&0xa000_0000u32.to_le_bytes());
            out.extend(h);
        }
        out.extend(items);
        out.extend(footer);
        out
    }
    #[test]
    fn validates_header_footer_items_and_bounds() {
        for header in [false, true] {
            let mut bytes = vec![42; 100];
            bytes.extend(tag(header, 70_000));
            let end = bytes.len() as u64;
            assert_eq!(audio_end(&mut Cursor::new(&bytes), 0, end).unwrap(), 100);
            for index in [
                end as usize - 20,
                end as usize - 16,
                end as usize - 8,
                100 + if header { 32 } else { 0 },
            ] {
                let mut bad = bytes.clone();
                bad[index] = 255;
                assert!(audio_end(&mut Cursor::new(&bad), 0, end).is_err());
            }
            assert!(audio_end(&mut Cursor::new(&bytes), 101, end).is_err());
        }
        assert_eq!(
            audio_end(&mut Cursor::new(vec![0; 100]), 0, 100).unwrap(),
            100
        );
    }
}
