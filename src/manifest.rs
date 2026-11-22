//! The package manifest (`PKGINFO`): a simple line-based `key = value`
//! format, deliberately dependency-free (no TOML crate).
//!
//! ```text
//! # RustOS package manifest
//! name = shell
//! version = 0.1.0
//! description = The RustOS command shell
//! depends = fs >=1.0.0, memory =0.1.0
//! file = bin/shell a1b2c3d4
//! file = lib/libshell.so 0123abcd
//! ```
//!
//! Keys:
//!
//! * `name` (required): package name, see [`validate_package_name`].
//! * `version` (required): `major.minor.patch`.
//! * `description` (optional): one line of free text.
//! * `depends` (optional): comma-separated `name <req>` entries.
//! * `file` (zero or more): `<relative-path> <crc32-hex>`; the checksum
//!   may be omitted in a source directory — `pack` fills it in from the
//!   file bytes and rejects a present-but-wrong checksum. Inside a
//!   `.rpk` archive the checksum is always present.
//!
//! Lines starting with `#` are comments; blank lines are ignored.
//! Unknown keys, duplicate scalar keys, absolute paths and `..`
//! components in file paths are all rejected.

use crate::version::{validate_package_name, Dependency, Version};

/// One file listed in a manifest, with its CRC-32/IEEE checksum
/// (`None` only in a hand-written source manifest before packing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub crc32: Option<u32>,
}

/// A parsed package manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: Version,
    pub description: String,
    pub depends: Vec<Dependency>,
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    pub fn new(name: &str, version: Version) -> Self {
        Self {
            name: name.to_string(),
            version,
            description: String::new(),
            depends: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Serialise to canonical `PKGINFO` text. The order is fixed
    /// (name, version, description, depends, files) so a round-trip
    /// through [`Manifest::parse`] is stable.
    pub fn serialise(&self) -> String {
        let mut out = String::new();
        out.push_str("# RustOS package manifest\n");
        out.push_str(&format!("name = {}\n", self.name));
        out.push_str(&format!("version = {}\n", self.version));
        if !self.description.is_empty() {
            out.push_str(&format!("description = {}\n", self.description));
        }
        if !self.depends.is_empty() {
            let deps: Vec<String> = self.depends.iter().map(|d| d.to_string()).collect();
            out.push_str(&format!("depends = {}\n", deps.join(", ")));
        }
        for f in &self.files {
            match f.crc32 {
                Some(crc) => out.push_str(&format!("file = {} {:08x}\n", f.path, crc)),
                None => out.push_str(&format!("file = {}\n", f.path)),
            }
        }
        out
    }

    /// Parse manifest text. Returns an error (never a panic) on any
    /// malformed input.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut name: Option<String> = None;
        let mut version: Option<Version> = None;
        let mut description = String::new();
        let mut depends: Vec<Dependency> = Vec::new();
        let mut files: Vec<ManifestFile> = Vec::new();
        let mut seen_scalar: Vec<&str> = Vec::new();

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => {
                    return Err(format!(
                        "manifest line {}: expected `key = value`, got {:?}",
                        lineno + 1,
                        line
                    ))
                }
            };
            match key {
                "name" | "version" | "description" | "depends" => {
                    if seen_scalar.contains(&key) {
                        return Err(format!(
                            "manifest line {}: duplicate key {:?}",
                            lineno + 1,
                            key
                        ));
                    }
                    seen_scalar.push(key);
                    match key {
                        "name" => {
                            validate_package_name(value).map_err(|e| {
                                format!("manifest line {}: {}", lineno + 1, e)
                            })?;
                            name = Some(value.to_string());
                        }
                        "version" => {
                            version = Some(value.parse::<Version>().map_err(|e| {
                                format!("manifest line {}: {}", lineno + 1, e)
                            })?);
                        }
                        "description" => description = value.to_string(),
                        "depends" => {
                            if !value.is_empty() {
                                for part in value.split(',') {
                                    let part = part.trim();
                                    if part.is_empty() {
                                        return Err(format!(
                                            "manifest line {}: empty entry in depends list",
                                            lineno + 1
                                        ));
                                    }
                                    depends.push(Dependency::parse(part).map_err(|e| {
                                        format!("manifest line {}: {}", lineno + 1, e)
                                    })?);
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                "file" => {
                    let mut parts = value.split_whitespace();
                    let path = parts.next().ok_or_else(|| {
                        format!("manifest line {}: `file` needs a path", lineno + 1)
                    })?;
                    validate_manifest_path(path).map_err(|e| {
                        format!("manifest line {}: {}", lineno + 1, e)
                    })?;
                    let crc = match parts.next() {
                        Some(hex) => Some(parse_crc32_hex(hex).map_err(|e| {
                            format!("manifest line {}: {}", lineno + 1, e)
                        })?),
                        None => None,
                    };
                    if parts.next().is_some() {
                        return Err(format!(
                            "manifest line {}: too many fields in `file` entry",
                            lineno + 1
                        ));
                    }
                    if files.iter().any(|f: &ManifestFile| f.path == path) {
                        return Err(format!(
                            "manifest line {}: duplicate file {:?}",
                            lineno + 1, path
                        ));
                    }
                    files.push(ManifestFile { path: path.to_string(), crc32: crc });
                }
                other => {
                    return Err(format!(
                        "manifest line {}: unknown key {:?}",
                        lineno + 1, other
                    ))
                }
            }
        }
        let name = name.ok_or_else(|| "manifest is missing required key `name`".to_string())?;
        let version =
            version.ok_or_else(|| "manifest is missing required key `version`".to_string())?;
        Ok(Manifest { name, version, description, depends, files })
    }
}

/// Validate a file path inside a package: relative, `/`-separated,
/// no empty components, no `.` or `..`, no backslashes, no drive
/// letters. (Paths are stored with `/` and converted to the host
/// separator on install.)
pub fn validate_manifest_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("file path is empty".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("file path {:?} must be relative", path));
    }
    if path.contains('\\') {
        return Err(format!("file path {:?} must use '/' separators", path));
    }
    if path.contains(':') {
        return Err(format!("file path {:?} must not contain ':'", path));
    }
    for part in path.split('/') {
        if part.is_empty() {
            return Err(format!("file path {:?} has an empty component", path));
        }
        if part == "." || part == ".." {
            return Err(format!("file path {:?} must not contain {:?}", path, part));
        }
    }
    Ok(())
}

/// Parse an 8-digit lowercase-or-uppercase hex CRC-32.
pub fn parse_crc32_hex(s: &str) -> Result<u32, String> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("checksum {:?} is not 8 hex digits", s));
    }
    u32::from_str_radix(s, 16).map_err(|e| format!("bad checksum {:?}: {}", s, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionReq;

    fn sample() -> Manifest {
        let mut m = Manifest::new("shell", Version::new(0, 1, 0));
        m.description = "The RustOS shell".to_string();
        m.depends = vec![Dependency::new("fs", VersionReq::GreaterOrEqual(Version::new(1, 0, 0)))];
        m.files = vec![
            ManifestFile { path: "bin/shell".to_string(), crc32: Some(0xa1b2_c3d4) },
            ManifestFile { path: "lib/libshell.so".to_string(), crc32: Some(0x0123_abcd) },
        ];
        m
    }

    #[test]
    fn manifest_roundtrip() {
        let m = sample();
        let text = m.serialise();
        let back = Manifest::parse(&text).unwrap();
        assert_eq!(back, m);
        // Serialising the parsed copy is byte-identical (canonical form).
        assert_eq!(back.serialise(), text);
    }

    #[test]
    fn manifest_minimal_roundtrip() {
        let m = Manifest::new("tiny", Version::new(1, 0, 0));
        let back = Manifest::parse(&m.serialise()).unwrap();
        assert_eq!(back, m);
        assert!(back.description.is_empty());
        assert!(back.depends.is_empty());
        assert!(back.files.is_empty());
    }

    #[test]
    fn manifest_comments_and_blank_lines() {
        let text = "# comment\n\nname = shell\n# another\nversion = 0.1.0\n";
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.name, "shell");
    }

    #[test]
    fn manifest_rejects_malformed() {
        // Missing required keys.
        assert!(Manifest::parse("version = 1.0.0\n").is_err());
        assert!(Manifest::parse("name = shell\n").is_err());
        assert!(Manifest::parse("").is_err());
        // Bad version.
        assert!(Manifest::parse("name = shell\nversion = 1.0\n").is_err());
        // Duplicate scalar key.
        assert!(Manifest::parse("name = a\nname = b\nversion = 1.0.0\n").is_err());
        // Unknown key.
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nbogus = 1\n").is_err());
        // Line without '='.
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nhello\n").is_err());
        // Bad package name.
        assert!(Manifest::parse("name = bad name\nversion = 1.0.0\n").is_err());
        // Bad dependency.
        assert!(Manifest::parse("name = a\nversion = 1.0.0\ndepends = fs =>1.0.0\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\ndepends = fs >=1.0.0,, net\n").is_err());
        // Bad file lines.
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = /etc/passwd 0123abcd\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = ../escape 0123abcd\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = a//b 0123abcd\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = a\\b 0123abcd\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = x.txt zz\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = x.txt 0123abcd extra\n").is_err());
        assert!(Manifest::parse("name = a\nversion = 1.0.0\nfile = x.txt 0123abcd\nfile = x.txt 0123abcd\n").is_err());
        // File without checksum is fine (pack fills it in).
        let m = Manifest::parse("name = a\nversion = 1.0.0\nfile = x.txt\n").unwrap();
        assert_eq!(m.files[0].crc32, None);
    }

    #[test]
    fn crc_hex_parse() {
        assert_eq!(parse_crc32_hex("cbf43926").unwrap(), 0xCBF4_3926);
        assert_eq!(parse_crc32_hex("CBF43926").unwrap(), 0xCBF4_3926);
        assert!(parse_crc32_hex("cbf4392").is_err());
        assert!(parse_crc32_hex("cbf439267").is_err());
        assert!(parse_crc32_hex("zzzzzzzz").is_err());
    }
}
