//! A minimal, reproducible ZIP writer — stored entries only.
//!
//! The sandbox export bundle (design's Plan 4) is a zip a static host serves
//! verbatim: `seed/site/*`, `seed/blocks/*.wasm`, `seed/data.json`. Nothing in
//! that set benefits from DEFLATE — wasm modules and already-minified assets
//! barely shrink, and a browser unzipping on demand doesn't need to pay a
//! decompression cost either — so this writer only ever emits method-0
//! (stored) entries, which keeps the whole implementation to the three record
//! types the ZIP format defines (local file header, central directory header,
//! end-of-central-directory) with no compressor pulled in.
//!
//! Every entry carries the same fixed DOS date/time (2026-01-01 00:00:00):
//! two exports of the same tree must produce byte-identical archives, and the
//! wall-clock the export ran at is not part of that identity.
//!
//! No ZIP64 — entries and the archive as a whole are refused once they would
//! carry the classic format's `u32` offset/size fields past 4 GiB. The
//! sandbox's own per-file and per-workspace quotas ([`super::paths`]) keep a
//! real export orders of magnitude under that; this is a hard backstop, not a
//! limit anyone is expected to hit.

use std::collections::HashSet;

/// DOS time field for 00:00:00 — the fixed timestamp every entry carries.
const DOS_TIME: u16 = 0;

/// DOS date field for 2026-01-01: `((2026 - 1980) << 9) | (1 << 5) | 1`.
const DOS_DATE: u16 = 0x5C21;

/// "Version [made by / needed to extract]" — 2.0, the floor for the
/// UTF-8-filename flag this writer always sets.
const VERSION: u16 = 20;

/// General-purpose bit flag: bit 11 (`0x0800`) marks the file name as UTF-8,
/// so a reader trusts `path` verbatim instead of guessing an OEM code page.
const FLAG_UTF8: u16 = 0x0800;

/// Compression method 0 — stored, no compression. See the module docs for
/// why this writer never emits anything else.
const METHOD_STORED: u16 = 0;

const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

/// Fixed size of a local file header, before the file name.
const LOCAL_HEADER_FIXED_LEN: usize = 30;

/// Ceiling every offset/size field in the classic (non-ZIP64) format can
/// hold.
const MAX_ARCHIVE_BYTES: usize = u32::MAX as usize;

/// Failure adding one entry to a [`ZipWriter`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZipError {
    /// `path` is absolute, uses `\`, or is otherwise not the archive-relative,
    /// forward-slash form every entry must use.
    #[error(
        "{0:?} is not a valid zip entry path — it must be relative and forward-slash-separated"
    )]
    BadPath(String),
    /// `path` was already added to this archive.
    #[error("{0:?} was already added to this archive")]
    Duplicate(String),
    /// `path`'s UTF-8 byte length does not fit the format's 16-bit name-length
    /// field (max 65535).
    #[error("{0:?} is longer than the 65535-byte zip entry name limit")]
    PathTooLong(String),
    /// Adding this entry would carry the archive past the 4 GiB ceiling the
    /// classic (non-ZIP64) format's `u32` offset/size fields impose.
    #[error("archive would exceed the 4 GiB limit this stored-entry writer supports (no ZIP64)")]
    TooLarge,
}

/// What [`ZipWriter::finish`] needs to remember about one entry to write its
/// central directory record. The local header's own copy of the same facts
/// is written immediately by [`ZipWriter::add`] and not re-derived from this
/// — this is purely the second record [`finish`](ZipWriter::finish) owes.
struct CentralEntry {
    name: String,
    crc32: u32,
    size: u32,
    offset: u32,
}

/// Builds a ZIP archive of stored (uncompressed) entries, byte-for-byte
/// reproducible across runs for the same inputs in the same order. See the
/// module docs for the format and reproducibility rationale.
pub struct ZipWriter {
    buf: Vec<u8>,
    entries: Vec<CentralEntry>,
    /// Mirrors `entries`' names as a set so [`ZipWriter::add`]'s duplicate
    /// check is O(1) rather than an O(n) scan of `entries` per call — an
    /// export bundle can carry thousands of source files.
    names: HashSet<String>,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    /// An archive with no entries.
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            entries: Vec::new(),
            names: HashSet::new(),
        }
    }

    /// Add one stored entry. `path` must be relative (no leading `/`),
    /// forward-slash-separated (no `\`), unique within this archive, and at
    /// most 65535 UTF-8 bytes long.
    ///
    /// Every check runs, and every fallible size cast happens, before
    /// anything is written or recorded — a rejected `add` leaves the writer
    /// exactly as it was, so a caller that wraps this in its own retry logic
    /// never has to reason about a half-applied entry.
    pub fn add(&mut self, path: &str, bytes: &[u8]) -> Result<(), ZipError> {
        if path.is_empty() || path.starts_with('/') || path.contains('\\') {
            return Err(ZipError::BadPath(path.to_string()));
        }
        if path.len() > u16::MAX as usize {
            return Err(ZipError::PathTooLong(path.to_string()));
        }
        if self.names.contains(path) {
            return Err(ZipError::Duplicate(path.to_string()));
        }
        let offset = u32::try_from(self.buf.len()).map_err(|_| ZipError::TooLarge)?;
        let size = u32::try_from(bytes.len()).map_err(|_| ZipError::TooLarge)?;
        let grows_by = LOCAL_HEADER_FIXED_LEN + path.len() + bytes.len();
        if self.buf.len().saturating_add(grows_by) > MAX_ARCHIVE_BYTES {
            return Err(ZipError::TooLarge);
        }

        let crc32 = crc32fast::hash(bytes);
        write_local_header(&mut self.buf, path, crc32, size);
        self.buf.extend_from_slice(bytes);

        self.names.insert(path.to_string());
        self.entries.push(CentralEntry {
            name: path.to_string(),
            crc32,
            size,
            offset,
        });
        Ok(())
    }

    /// Consume the writer and return the complete archive: every entry's
    /// local header and data (already written by `add`), then the central
    /// directory, then the end-of-central-directory record.
    pub fn finish(self) -> Vec<u8> {
        let mut buf = self.buf;
        let central_start = buf.len();
        for entry in &self.entries {
            write_central_header(&mut buf, entry);
        }
        let central_size = buf.len() - central_start;
        write_eocd(&mut buf, self.entries.len(), central_size, central_start);
        buf
    }
}

/// Local file header (`PK\x03\x04`) plus the entry's raw bytes — everything
/// [`ZipWriter::add`] writes immediately, ahead of the central directory.
fn write_local_header(buf: &mut Vec<u8>, path: &str, crc32: u32, size: u32) {
    buf.extend_from_slice(&LOCAL_HEADER_SIG.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
    buf.extend_from_slice(&METHOD_STORED.to_le_bytes());
    buf.extend_from_slice(&DOS_TIME.to_le_bytes());
    buf.extend_from_slice(&DOS_DATE.to_le_bytes());
    buf.extend_from_slice(&crc32.to_le_bytes());
    // Stored entries have no compression, so compressed size == uncompressed.
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&(path.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    buf.extend_from_slice(path.as_bytes());
}

/// One central directory record (`PK\x01\x02`) — the same facts the local
/// header carries, plus the offset a reader needs to seek to it.
fn write_central_header(buf: &mut Vec<u8>, entry: &CentralEntry) {
    buf.extend_from_slice(&CENTRAL_HEADER_SIG.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes()); // version made by
    buf.extend_from_slice(&VERSION.to_le_bytes()); // version needed to extract
    buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
    buf.extend_from_slice(&METHOD_STORED.to_le_bytes());
    buf.extend_from_slice(&DOS_TIME.to_le_bytes());
    buf.extend_from_slice(&DOS_DATE.to_le_bytes());
    buf.extend_from_slice(&entry.crc32.to_le_bytes());
    buf.extend_from_slice(&entry.size.to_le_bytes());
    buf.extend_from_slice(&entry.size.to_le_bytes());
    buf.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    buf.extend_from_slice(&0u16.to_le_bytes()); // file comment length
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk number start
    buf.extend_from_slice(&0u16.to_le_bytes()); // internal file attributes
    buf.extend_from_slice(&0u32.to_le_bytes()); // external file attributes
    buf.extend_from_slice(&entry.offset.to_le_bytes());
    buf.extend_from_slice(entry.name.as_bytes());
}

/// The end-of-central-directory record (`PK\x05\x06`) — the last thing a zip
/// reader looks for, since it is what points back at everything else.
fn write_eocd(buf: &mut Vec<u8>, entry_count: usize, central_size: usize, central_offset: usize) {
    buf.extend_from_slice(&EOCD_SIG.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // number of this disk
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk where central directory starts
    buf.extend_from_slice(&(entry_count as u16).to_le_bytes());
    buf.extend_from_slice(&(entry_count as u16).to_le_bytes());
    buf.extend_from_slice(&(central_size as u32).to_le_bytes());
    buf.extend_from_slice(&(central_offset as u32).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment length
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_reads_back_with_the_zip_crate() {
        let mut w = ZipWriter::new();
        w.add("README.md", b"hello").unwrap();
        w.add("seed/site/index.html", b"<h1>x</h1>").unwrap();
        w.add("seed/blocks/hello.wasm", &[0, 97, 115, 109, 1, 0, 0, 0])
            .unwrap();
        let bytes = w.finish();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 3);
        let mut f = archive.by_name("seed/site/index.html").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "<h1>x</h1>");
        assert_eq!(f.compression(), zip::CompressionMethod::Stored);
        assert_eq!(f.crc32(), crc32fast::hash(b"<h1>x</h1>"));
    }

    #[test]
    fn duplicate_and_absolute_paths_are_rejected() {
        let mut w = ZipWriter::new();
        w.add("a", b"1").unwrap();
        assert!(matches!(w.add("a", b"2"), Err(ZipError::Duplicate(_))));
        assert!(matches!(w.add("/a", b"2"), Err(ZipError::BadPath(_))));
        assert!(matches!(w.add("a\\b", b"2"), Err(ZipError::BadPath(_))));
    }
}
