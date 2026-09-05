use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Result, anyhow};
use secrecy::SecretBox;

use crate::block_cipher::{BLOCK_SIZE, BlockCipher, FULL_BLOCK_DISK_SIZE, NONCE_LEN, TAG_LEN};
use crate::keys::ContentKey;

pub const MAGIC: &[u8; 4] = b"MKV1";
pub const HEADER_SIZE: u64 = 4 + 1 + 16;

/// Creates a new, empty vault-format file: just the header, zero blocks.
pub fn create_empty_file(path: &Path, file_id: &[u8; 16]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut buf = Vec::with_capacity(HEADER_SIZE as usize);
    buf.extend_from_slice(MAGIC);
    buf.push(1u8);
    buf.extend_from_slice(file_id);
    file.write_all(&buf)?;
    Ok(())
}

pub fn read_file_id(file: &mut File) -> Result<[u8; 16]> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; HEADER_SIZE as usize];
    file.read_exact(&mut buf).map_err(|_| anyhow!("corrupt vault file: header unreadable"))?;
    if &buf[0..4] != MAGIC {
        return Err(anyhow!("corrupt vault file: bad magic"));
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&buf[5..21]);
    Ok(id)
}

/// (logical_size, num_blocks) derived purely from the file's physical
/// length on disk — no separate size field is persisted (plan §1).
pub fn logical_layout(physical_len: u64) -> (u64, u64) {
    if physical_len <= HEADER_SIZE {
        return (0, 0);
    }
    let remaining = physical_len - HEADER_SIZE;
    let full_blocks = remaining / FULL_BLOCK_DISK_SIZE;
    let rem = remaining % FULL_BLOCK_DISK_SIZE;
    if rem == 0 {
        (full_blocks * BLOCK_SIZE as u64, full_blocks)
    } else {
        let overhead = (NONCE_LEN + TAG_LEN) as u64;
        let last_plain_len = rem.saturating_sub(overhead);
        (full_blocks * BLOCK_SIZE as u64 + last_plain_len, full_blocks + 1)
    }
}

fn block_offset(idx: u64) -> u64 {
    HEADER_SIZE + idx * FULL_BLOCK_DISK_SIZE
}

fn block_disk_len(idx: u64, num_blocks: u64, physical_len: u64) -> u64 {
    if idx + 1 < num_blocks {
        FULL_BLOCK_DISK_SIZE
    } else {
        physical_len - block_offset(idx)
    }
}

fn read_block_plain(
    file: &mut File,
    cipher: &BlockCipher,
    file_id: &[u8; 16],
    idx: u64,
    disk_len: u64,
) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(block_offset(idx)))?;
    let mut disk_bytes = vec![0u8; disk_len as usize];
    file.read_exact(&mut disk_bytes)?;
    cipher.decrypt_block(file_id, idx, &disk_bytes)
}

/// Reads up to `buf.len()` bytes starting at `offset`, clamped to the
/// file's logical size. Returns the number of bytes actually read.
pub fn read_range(
    file: &mut File,
    content_key: &SecretBox<ContentKey>,
    file_id: &[u8; 16],
    offset: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let physical_len = file.metadata()?.len();
    let (logical_size, num_blocks) = logical_layout(physical_len);
    if offset >= logical_size || buf.is_empty() {
        return Ok(0);
    }
    let cipher = BlockCipher::new(content_key);
    let end = (offset + buf.len() as u64).min(logical_size);
    let mut written = 0usize;
    let mut pos = offset;
    while pos < end {
        let block_idx = pos / BLOCK_SIZE as u64;
        let in_block_off = (pos % BLOCK_SIZE as u64) as usize;
        let disk_len = block_disk_len(block_idx, num_blocks, physical_len);
        let plain = read_block_plain(file, &cipher, file_id, block_idx, disk_len)?;
        let avail = plain.len().saturating_sub(in_block_off);
        let want = ((end - pos) as usize).min(avail);
        if want == 0 {
            break;
        }
        buf[written..written + want].copy_from_slice(&plain[in_block_off..in_block_off + want]);
        written += want;
        pos += want as u64;
    }
    Ok(written)
}

/// Overlays `data` (written at absolute `write_offset`) onto `block`
/// (already resized to its target plaintext length), for the portion that
/// falls within `block_idx`'s logical byte range.
fn patch_block(block: &mut [u8], block_idx: u64, write_offset: u64, data: &[u8]) {
    let block_logical_start = block_idx * BLOCK_SIZE as u64;
    let block_logical_end = block_logical_start + block.len() as u64;
    let write_end = write_offset + data.len() as u64;

    let overlap_start = write_offset.max(block_logical_start);
    let overlap_end = write_end.min(block_logical_end);
    if overlap_start >= overlap_end {
        return;
    }
    let src_start = (overlap_start - write_offset) as usize;
    let src_end = (overlap_end - write_offset) as usize;
    let dst_start = (overlap_start - block_logical_start) as usize;
    let dst_end = (overlap_end - block_logical_start) as usize;
    block[dst_start..dst_end].copy_from_slice(&data[src_start..src_end]);
}

/// Writes `data` at `offset`, zero-filling any gap and growing the file if
/// needed. Returns (bytes_written, physical_len_before, physical_len_after)
/// so the caller can adjust quota accounting (plan §2/§4).
pub fn write_range(
    file: &mut File,
    content_key: &SecretBox<ContentKey>,
    file_id: &[u8; 16],
    offset: u64,
    data: &[u8],
) -> Result<(usize, u64, u64)> {
    let physical_before = file.metadata()?.len();
    if data.is_empty() {
        return Ok((0, physical_before, physical_before));
    }
    let (logical_size, num_blocks) = logical_layout(physical_before);
    let cipher = BlockCipher::new(content_key);

    let write_end = offset
        .checked_add(data.len() as u64)
        .ok_or_else(|| anyhow!("write offset overflow"))?;
    let start_block = offset / BLOCK_SIZE as u64;
    let end_block = (write_end - 1) / BLOCK_SIZE as u64;
    let last_existing_block = if num_blocks == 0 { None } else { Some(num_blocks - 1) };

    let tail_affected = match last_existing_block {
        Some(last) => end_block >= last,
        None => true,
    };

    if !tail_affected {
        // Pure interior overwrite: every touched block is guaranteed to
        // already be a full 4096-byte block (later blocks exist), so its
        // on-disk size never changes — safe to rewrite in place.
        for block_idx in start_block..=end_block {
            let disk_len = block_disk_len(block_idx, num_blocks, physical_before);
            let mut plain = read_block_plain(file, &cipher, file_id, block_idx, disk_len)?;
            plain.resize(BLOCK_SIZE, 0);
            patch_block(&mut plain, block_idx, offset, data);
            let encrypted = cipher.encrypt_block(file_id, block_idx, &plain)?;
            debug_assert_eq!(encrypted.len() as u64, FULL_BLOCK_DISK_SIZE);
            file.seek(SeekFrom::Start(block_offset(block_idx)))?;
            file.write_all(&encrypted)?;
        }
        return Ok((data.len(), physical_before, physical_before));
    }

    // Tail is affected: blocks from `tail_start` onward may change size, so
    // truncate from there and re-append everything through the new end.
    // `unwrap_or(0)` (not `start_block`) matters here: with no existing
    // blocks at all, every block from 0 must be (re)written, or the
    // physical layout would have a gap the contiguous-block scheme can't
    // represent.
    let tail_start = last_existing_block.map(|l| l.min(start_block)).unwrap_or(0);
    let new_logical_size = logical_size.max(write_end);
    let new_last_block = (new_logical_size - 1) / BLOCK_SIZE as u64;

    let mut appended = Vec::new();
    for block_idx in tail_start..=new_last_block {
        let block_logical_start = block_idx * BLOCK_SIZE as u64;
        let this_block_len = (new_logical_size - block_logical_start).min(BLOCK_SIZE as u64) as usize;

        let mut plain = if block_idx < num_blocks {
            let disk_len = block_disk_len(block_idx, num_blocks, physical_before);
            read_block_plain(file, &cipher, file_id, block_idx, disk_len)?
        } else {
            Vec::new()
        };
        plain.resize(this_block_len, 0);
        patch_block(&mut plain, block_idx, offset, data);
        let encrypted = cipher.encrypt_block(file_id, block_idx, &plain)?;
        appended.extend_from_slice(&encrypted);
    }

    file.set_len(block_offset(tail_start))?;
    file.seek(SeekFrom::Start(block_offset(tail_start)))?;
    file.write_all(&appended)?;

    let physical_after = block_offset(tail_start) + appended.len() as u64;
    Ok((data.len(), physical_before, physical_after))
}

/// Truncates/extends the file to exactly `new_size` logical bytes.
/// Returns (physical_len_before, physical_len_after).
pub fn set_len(
    file: &mut File,
    content_key: &SecretBox<ContentKey>,
    file_id: &[u8; 16],
    new_size: u64,
) -> Result<(u64, u64)> {
    let physical_before = file.metadata()?.len();
    let (logical_size, old_num_blocks) = logical_layout(physical_before);

    if new_size == logical_size {
        return Ok((physical_before, physical_before));
    }

    if new_size == 0 {
        file.set_len(HEADER_SIZE)?;
        return Ok((physical_before, HEADER_SIZE));
    }

    if new_size > logical_size {
        // Extend with zero bytes via the same tail-rewrite path as a write
        // of empty-ish data past the current end.
        let filler_offset = logical_size;
        let filler = vec![0u8; (new_size - logical_size) as usize];
        let (_, _, physical_after) = write_range(file, content_key, file_id, filler_offset, &filler)?;
        return Ok((physical_before, physical_after));
    }

    // Shrinking: keep whole blocks up to new_size, and if new_size lands
    // mid-block, truncate that block's plaintext before re-encrypting it.
    let cipher = BlockCipher::new(content_key);
    let new_num_blocks = new_size.div_ceil(BLOCK_SIZE as u64);
    let last_kept = new_num_blocks - 1;
    let remainder = (new_size - last_kept * BLOCK_SIZE as u64) as usize;

    let disk_len = block_disk_len(last_kept, old_num_blocks, physical_before);
    let mut plain = read_block_plain(file, &cipher, file_id, last_kept, disk_len)?;
    plain.truncate(remainder);
    let encrypted = cipher.encrypt_block(file_id, last_kept, &plain)?;

    file.set_len(block_offset(last_kept))?;
    file.seek(SeekFrom::Start(block_offset(last_kept)))?;
    file.write_all(&encrypted)?;

    let physical_after = block_offset(last_kept) + encrypted.len() as u64;
    Ok((physical_before, physical_after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretBox;
    use tempfile::tempdir;

    fn key() -> SecretBox<ContentKey> {
        SecretBox::new(Box::new(ContentKey([3u8; 32])))
    }

    fn open_rw(path: &Path) -> File {
        OpenOptions::new().read(true).write(true).open(path).unwrap()
    }

    #[test]
    fn test_empty_file_has_zero_logical_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        create_empty_file(&path, &[1u8; 16]).unwrap();
        let (size, blocks) = logical_layout(std::fs::metadata(&path).unwrap().len());
        assert_eq!(size, 0);
        assert_eq!(blocks, 0);
    }

    #[test]
    fn test_write_then_read_small_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [1u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, b"hello world").unwrap();

        let mut buf = [0u8; 11];
        let n = read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"hello world");
    }

    #[test]
    fn test_write_spanning_multiple_blocks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [2u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let data: Vec<u8> = (0..(BLOCK_SIZE * 3 + 500)).map(|i| (i % 256) as u8).collect();
        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, &data).unwrap();

        let mut buf = vec![0u8; data.len()];
        let n = read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(buf, data);
    }

    #[test]
    fn test_interior_overwrite_does_not_disturb_other_blocks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [3u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let data = vec![0xAAu8; BLOCK_SIZE * 3];
        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, &data).unwrap();

        // Overwrite 10 bytes squarely inside block 1.
        write_range(&mut file, &k, &file_id, BLOCK_SIZE as u64 + 50, b"OVERWRITE!").unwrap();

        let mut buf = vec![0u8; data.len()];
        read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(&buf[BLOCK_SIZE + 50..BLOCK_SIZE + 60], b"OVERWRITE!");
        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[BLOCK_SIZE * 2 + 100], 0xAA);
    }

    #[test]
    fn test_sparse_write_zero_fills_gap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [4u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 10_000, b"end").unwrap();

        let mut buf = vec![0u8; 10_003];
        let n = read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(n, 10_003);
        assert!(buf[..10_000].iter().all(|&b| b == 0));
        assert_eq!(&buf[10_000..], b"end");
    }

    #[test]
    fn test_truncate_shrink_then_grow_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [5u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, &vec![0x11u8; BLOCK_SIZE * 2 + 200]).unwrap();

        set_len(&mut file, &k, &file_id, 10).unwrap();
        let (size, _) = logical_layout(file.metadata().unwrap().len());
        assert_eq!(size, 10);

        set_len(&mut file, &k, &file_id, 20).unwrap();
        let mut buf = vec![0u8; 20];
        read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(&buf[..10], &[0x11u8; 10]);
        assert_eq!(&buf[10..], &[0u8; 10]);
    }

    #[test]
    fn test_truncate_to_zero_then_write_again() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [6u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, b"some data here").unwrap();
        set_len(&mut file, &k, &file_id, 0).unwrap();
        assert_eq!(logical_layout(file.metadata().unwrap().len()).0, 0);

        write_range(&mut file, &k, &file_id, 0, b"fresh").unwrap();
        let mut buf = [0u8; 5];
        read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"fresh");
    }

    #[test]
    fn test_overwrite_across_old_eof_extends_correctly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f");
        let file_id = [7u8; 16];
        create_empty_file(&path, &file_id).unwrap();
        let k = key();

        let mut file = open_rw(&path);
        write_range(&mut file, &k, &file_id, 0, &vec![0x22u8; BLOCK_SIZE + 10]).unwrap();
        // Write starting before the old EOF and extending past it.
        write_range(&mut file, &k, &file_id, BLOCK_SIZE as u64, b"tail-extend-data").unwrap();

        let (size, _) = logical_layout(file.metadata().unwrap().len());
        assert_eq!(size, BLOCK_SIZE as u64 + "tail-extend-data".len() as u64);
        let mut buf = vec![0u8; size as usize];
        read_range(&mut file, &k, &file_id, 0, &mut buf).unwrap();
        assert_eq!(&buf[BLOCK_SIZE..], b"tail-extend-data");
    }
}
