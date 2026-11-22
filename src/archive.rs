//! The `.rpk` package archive format (std-only, no compression).
//!
//! Layout, all integers little-endian:
//!
//! ```text
//! "RPK1"                 magic, 4 bytes
//! u32 manifest_len
//! manifest_len bytes     canonical PKGINFO text (UTF-8)
//! for each file, in manifest order:
//!   u16 name_len
//!   name_len bytes       '/'-separated relative path (UTF-8)
//!   u64 data_len
//!   u32 crc32            CRC-32/IEEE of the file bytes
//!   data_len bytes       file bytes
//! ```
//!
//! The checksum stored in the manifest must agree with the checksum
//! stored in the file record and with the actual bytes; `verify` and
//! `unpack` check all three and fail closed on any mismatch. The CRC
//! is the same IEEE flavour as [`rustos_image::crc32_ieee`] (which
//! GPT uses), so a checksum computed here matches one computed there.

use crate::manifest::Manifest;
use std::fs;
use std::path::{Path, PathBuf};

/// CRC-32/IEEE, identical to `rustos_image::crc32_ieee` (poly
/// 0xEDB88320, init 0xFFFFFFFF, final xor). Known vector:
/// `crc32_ieee(b"123456789") == 0xCBF43926`.
pub use rustos_image::crc32_ieee;

pub const ARCHIVE_MAGIC: &[u8; 4] = b"RPK1";
pub const MANIFEST_FILE_NAME: &str = "PKGINFO";

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        if self.pos + n > self.data.len() {
            return Err(format!("archive is truncated while reading {}", what));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    fn u16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self, what: &str) -> Result<u32, String> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self, what: &str) -> Result<u64, String> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

/// Parse and fully verify archive bytes: magic, manifest, and every
/// file's name, length and CRC-32 against both the manifest and the
/// stored record. Returns the manifest and the file contents.
pub fn read_archive_bytes(bytes: &[u8]) -> Result<(Manifest, Vec<(String, Vec<u8>)>), String> {
    let mut cur = Cursor { data: bytes, pos: 0 };
    let magic = cur.take(4, "magic")?;
    if magic != ARCHIVE_MAGIC {
        return Err("not a RustOS package: bad magic (expected RPK1)".to_string());
    }
    let manifest_len = cur.u32("manifest length")? as usize;
    let manifest_bytes = cur.take(manifest_len, "manifest")?;
    let manifest_text =
        std::str::from_utf8(manifest_bytes).map_err(|e| format!("manifest is not UTF-8: {}", e))?;
    let manifest = Manifest::parse(manifest_text)?;
    let mut files = Vec::new();
    for expected in &manifest.files {
        let name_len = cur.u16("file name length")? as usize;
        let name_bytes = cur.take(name_len, "file name")?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|e| format!("file name is not UTF-8: {}", e))?
            .to_string();
        if name != expected.path {
            return Err(format!(
                "archive file order/name mismatch: manifest lists {:?}, archive holds {:?}",
                expected.path, name
            ));
        }
        let data_len = cur.u64("file length")? as usize;
        let stored_crc = cur.u32("file crc32")?;
        let data = cur.take(data_len, &format!("file {:?}", name))?.to_vec();
        let actual_crc = crc32_ieee(&data);
        if actual_crc != stored_crc {
            return Err(format!(
                "checksum mismatch for {:?}: stored {:08x}, computed {:08x}",
                name, stored_crc, actual_crc
            ));
        }
        match expected.crc32 {
            Some(manifest_crc) if manifest_crc != actual_crc => {
                return Err(format!(
                    "manifest checksum mismatch for {:?}: manifest {:08x}, computed {:08x}",
                    name, manifest_crc, actual_crc
                ));
            }
            _ => {}
        }
        files.push((name, data));
    }
    if cur.pos != bytes.len() {
        return Err(format!(
            "archive has {} trailing bytes after the last file",
            bytes.len() - cur.pos
        ));
    }
    Ok((manifest, files))
}

/// Read and verify an archive file from disk.
pub fn read_archive(path: &Path) -> Result<(Manifest, Vec<(String, Vec<u8>)>), String> {
    let bytes = fs::read(path)
        .map_err(|e| format!("cannot read package {}: {}", path.display(), e))?;
    read_archive_bytes(&bytes).map_err(|e| format!("{}: {}", path.display(), e))
}

/// Verify an archive without extracting it. Returns its manifest.
pub fn verify_archive(path: &Path) -> Result<Manifest, String> {
    Ok(read_archive(path)?.0)
}

/// Build a `.rpk` package from a source directory.
///
/// The directory must contain a `PKGINFO` manifest plus every file the
/// manifest lists. Manifest `file` lines may omit the checksum — pack
/// computes it — but a present checksum that disagrees with the file
/// bytes is an error. Files in the directory that the manifest does
/// not list are ignored. The archive stores the canonical manifest
/// with all checksums filled in.
pub fn pack(src_dir: &Path, out_path: &Path) -> Result<Manifest, String> {
    let manifest_path = src_dir.join(MANIFEST_FILE_NAME);
    let text = fs::read_to_string(&manifest_path).map_err(|e| {
        format!("cannot read manifest {}: {}", manifest_path.display(), e)
    })?;
    let mut manifest = Manifest::parse(&text)?;
    let mut out = Vec::new();
    out.extend_from_slice(ARCHIVE_MAGIC);
    // Serialise the manifest only after checksums are filled in.
    let mut payload: Vec<(String, Vec<u8>)> = Vec::new();
    for file in &mut manifest.files {
        let full = join_manifest_path(src_dir, &file.path);
        let data = fs::read(&full).map_err(|e| {
            format!("cannot read package file {}: {}", full.display(), e)
        })?;
        let crc = crc32_ieee(&data);
        match file.crc32 {
            Some(expected) if expected != crc => {
                return Err(format!(
                    "manifest checksum mismatch for {:?}: manifest {:08x}, file {:08x}",
                    file.path, expected, crc
                ));
            }
            _ => file.crc32 = Some(crc),
        }
        payload.push((file.path.clone(), data));
    }
    let manifest_text = manifest.serialise();
    put_u32(&mut out, manifest_text.len() as u32);
    out.extend_from_slice(manifest_text.as_bytes());
    for (name, data) in &payload {
        put_u16(&mut out, name.len() as u16);
        out.extend_from_slice(name.as_bytes());
        put_u64(&mut out, data.len() as u64);
        put_u32(&mut out, crc32_ieee(data));
        out.extend_from_slice(data);
    }
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create output dir {}: {}", parent.display(), e))?;
        }
    }
    fs::write(out_path, &out)
        .map_err(|e| format!("cannot write package {}: {}", out_path.display(), e))?;
    Ok(manifest)
}

/// Verify an archive and extract its files into `dest_dir`
/// (created if needed). Returns the manifest.
pub fn unpack_archive(archive_path: &Path, dest_dir: &Path) -> Result<Manifest, String> {
    let (manifest, files) = read_archive(archive_path)?;
    for (name, data) in &files {
        let dest = join_manifest_path(dest_dir, name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("cannot create dir {}: {}", parent.display(), e)
            })?;
        }
        fs::write(&dest, data)
            .map_err(|e| format!("cannot write {}: {}", dest.display(), e))?;
    }
    Ok(manifest)
}

/// Join a validated manifest path (always `/`-separated) onto a host
/// directory, using the host separator.
pub fn join_manifest_path(base: &Path, manifest_path: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for part in manifest_path.split('/') {
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestFile;
    use crate::testutil::{make_package_dir, tmp_path};
    use crate::version::{Dependency, Version, VersionReq};

    #[test]
    fn crc32_known_vector_and_cross_compat() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
        // Same function rustos-image uses for GPT headers.
        assert_eq!(crc32_ieee(b"hello"), rustos_image::crc32_ieee(b"hello"));
    }

    fn sample_manifest() -> Manifest {
        let mut m = Manifest::new("shell", Version::new(0, 1, 0));
        m.description = "shell".to_string();
        m.depends = vec![Dependency::new(
            "fs",
            VersionReq::GreaterOrEqual(Version::new(1, 0, 0)),
        )];
        m.files = vec![
            ManifestFile { path: "bin/shell".to_string(), crc32: None },
            ManifestFile { path: "lib/libshell.so".to_string(), crc32: None },
        ];
        m
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let src = make_package_dir("shell", "0.1.0", &[("bin/shell", b"#!/bin/sh\n"), ("lib/libshell.so", b"\x7FELF fake")]);
        let out = tmp_path("shell-0.1.0.rpk");
        let manifest = pack(&src, &out).unwrap();
        assert_eq!(manifest.name, "shell");
        assert!(manifest.files.iter().all(|f| f.crc32.is_some()));
        // The archive's stored manifest has checksums filled in.
        let (m2, files) = read_archive(&out).unwrap();
        assert_eq!(m2, manifest);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "bin/shell");
        assert_eq!(files[0].1, b"#!/bin/sh\n");
        assert_eq!(files[1].1, b"\x7FELF fake");
        // Unpack to a fresh dir and compare bytes.
        let dest = tmp_path("unpacked");
        let m3 = unpack_archive(&out, &dest).unwrap();
        assert_eq!(m3, manifest);
        assert_eq!(fs::read(dest.join("bin/shell")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(fs::read(dest.join("lib/libshell.so")).unwrap(), b"\x7FELF fake");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn pack_fills_missing_checksums_and_rejects_wrong_ones() {
        // Manifest with a deliberately wrong checksum.
        let src = tmp_path("badcrc-src");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::write(src.join("bin/x"), b"data").unwrap();
        fs::write(
            src.join(MANIFEST_FILE_NAME),
            "name = bad\nversion = 1.0.0\nfile = bin/x 00000000\n",
        )
        .unwrap();
        let out = tmp_path("bad.rpk");
        let err = pack(&src, &out).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{}", err);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn verify_rejects_tampered_archive() {
        let src = make_package_dir("tamper", "1.0.0", &[("a.txt", b"original bytes")]);
        let out = tmp_path("tamper.rpk");
        pack(&src, &out).unwrap();
        let mut bytes = fs::read(&out).unwrap();
        // Flip a byte in the payload (last byte of the file data).
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let tampered = tmp_path("tampered.rpk");
        fs::write(&tampered, &bytes).unwrap();
        let err = read_archive(&tampered).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{}", err);
        // Truncation is also rejected.
        let truncated = tmp_path("trunc.rpk");
        fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        assert!(read_archive(&truncated).is_err());
        // Bad magic is rejected.
        let badmagic = tmp_path("badmagic.rpk");
        let mut b2 = bytes.clone();
        b2[0] = b'X';
        fs::write(&badmagic, &b2).unwrap();
        let err = read_archive(&badmagic).unwrap_err();
        assert!(err.contains("bad magic"), "{}", err);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
        let _ = fs::remove_file(&tampered);
        let _ = fs::remove_file(&truncated);
        let _ = fs::remove_file(&badmagic);
    }

    #[test]
    fn pack_requires_manifest_and_listed_files() {
        let empty = tmp_path("empty-src");
        fs::create_dir_all(&empty).unwrap();
        let out = tmp_path("empty.rpk");
        assert!(pack(&empty, &out).is_err());
        // Manifest lists a file that is not on disk.
        let src = tmp_path("missing-src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join(MANIFEST_FILE_NAME), "name = m\nversion = 1.0.0\nfile = nope.txt\n").unwrap();
        assert!(pack(&src, &out).is_err());
        let _ = fs::remove_dir_all(&empty);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn sample_manifest_packs() {
        // The in-code sample manifest (with checksums omitted) packs fine.
        let src = tmp_path("sample-src");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::create_dir_all(src.join("lib")).unwrap();
        fs::write(src.join(MANIFEST_FILE_NAME), sample_manifest().serialise()).unwrap();
        fs::write(src.join("bin/shell"), b"shell-bytes").unwrap();
        fs::write(src.join("lib/libshell.so"), b"so-bytes").unwrap();
        let out = tmp_path("sample.rpk");
        let m = pack(&src, &out).unwrap();
        assert_eq!(m, sample_manifest_with_crcs());
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
    }

    fn sample_manifest_with_crcs() -> Manifest {
        let mut m = sample_manifest();
        m.files[0].crc32 = Some(crc32_ieee(b"shell-bytes"));
        m.files[1].crc32 = Some(crc32_ieee(b"so-bytes"));
        m
    }
}
