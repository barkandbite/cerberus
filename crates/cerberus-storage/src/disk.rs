//! On-disk encoding for the data dir — ours, std-only, no serde.
//!
//! Binary files are a `CERB` magic + `u16` format version + `u16` kind tag,
//! followed by records. A record is a `u32`-LE field count, then per field a
//! `u32`-LE length and that many bytes. Writers are append-into-a-buffer; the
//! buffer lands on disk through [`atomic_write`] (sibling tmp file, fsync,
//! rename) so a crash never leaves a torn file, only the previous version.

use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"CERB";

/// File kinds (the `u16` after the version).
pub(crate) const KIND_COOKIES: u16 = 1;
pub(crate) const KIND_VAULT: u16 = 2;

pub(crate) const FORMAT_VERSION: u16 = 1;

/// Write `bytes` to `path` atomically: tmp sibling → fsync → rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Serializes records into an in-memory buffer (then [`atomic_write`] it).
pub(crate) struct RecordWriter {
    buf: Vec<u8>,
}

impl RecordWriter {
    pub(crate) fn new(kind: u16) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&kind.to_le_bytes());
        Self { buf }
    }

    /// Append one record (a list of byte fields).
    pub(crate) fn record(&mut self, fields: &[&[u8]]) {
        self.buf
            .extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for field in fields {
            self.buf
                .extend_from_slice(&(field.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(field);
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Iterates the records of a buffer produced by [`RecordWriter`].
pub(crate) struct RecordReader<'a> {
    rest: &'a [u8],
}

impl<'a> RecordReader<'a> {
    /// Validate the header; `kind` must match.
    pub(crate) fn new(bytes: &'a [u8], kind: u16) -> io::Result<Self> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err(bad("not a CERB file"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != FORMAT_VERSION {
            return Err(bad("unsupported format version"));
        }
        let got_kind = u16::from_le_bytes([bytes[6], bytes[7]]);
        if got_kind != kind {
            return Err(bad("wrong file kind"));
        }
        Ok(Self { rest: &bytes[8..] })
    }

    /// The next record, or `None` at a clean EOF. A torn/corrupt tail is an
    /// error (the atomic writer should make that impossible).
    pub(crate) fn next_record(&mut self) -> io::Result<Option<Vec<Vec<u8>>>> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "truncated record");
        let count = self.read_u32().ok_or_else(bad)?;
        let mut fields = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let len = self.read_u32().ok_or_else(bad)? as usize;
            if self.rest.len() < len {
                return Err(bad());
            }
            let (field, rest) = self.rest.split_at(len);
            fields.push(field.to_vec());
            self.rest = rest;
        }
        Ok(Some(fields))
    }

    fn read_u32(&mut self) -> Option<u32> {
        if self.rest.len() < 4 {
            return None;
        }
        let (n, rest) = self.rest.split_at(4);
        self.rest = rest;
        Some(u32::from_le_bytes([n[0], n[1], n[2], n[3]]))
    }
}

/// Read a whole file, distinguishing "absent" (Ok(None)) from real errors.
pub(crate) fn read_if_exists(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(Some(buf))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip() {
        let mut w = RecordWriter::new(KIND_COOKIES);
        w.record(&[b"alpha", b"", b"\x00\x01\x02"]);
        w.record(&[b"solo"]);
        let bytes = w.finish();

        let mut r = RecordReader::new(&bytes, KIND_COOKIES).unwrap();
        let rec1 = r.next_record().unwrap().unwrap();
        assert_eq!(rec1, vec![b"alpha".to_vec(), vec![], vec![0, 1, 2]]);
        let rec2 = r.next_record().unwrap().unwrap();
        assert_eq!(rec2, vec![b"solo".to_vec()]);
        assert!(r.next_record().unwrap().is_none());
    }

    #[test]
    fn header_validation_rejects_wrong_magic_version_kind() {
        let bytes = RecordWriter::new(KIND_COOKIES).finish();
        assert!(RecordReader::new(&bytes, KIND_VAULT).is_err());
        assert!(RecordReader::new(b"NOPE\x01\x00\x01\x00", KIND_COOKIES).is_err());
        let mut wrong_ver = bytes.clone();
        wrong_ver[4] = 0xFF;
        assert!(RecordReader::new(&wrong_ver, KIND_COOKIES).is_err());
    }

    #[test]
    fn truncated_tail_is_an_error_not_a_silent_eof() {
        let mut w = RecordWriter::new(KIND_COOKIES);
        w.record(&[b"field"]);
        let mut bytes = w.finish();
        bytes.truncate(bytes.len() - 2);
        let mut r = RecordReader::new(&bytes, KIND_COOKIES).unwrap();
        assert!(r.next_record().is_err());
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("cerb-disk-test-{}", std::process::id()));
        let path = dir.join("f.bin");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
