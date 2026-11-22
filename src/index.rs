//! The package index: a file listing the packages available for
//! install, with add/remove/query and a serialise/parse round-trip.
//!
//! Format (one `[pkg]` block per package, `#` comments, blank lines
//! ignored):
//!
//! ```text
//! # RustOS package index
//! [pkg]
//! name = shell
//! version = 0.1.0
//! description = The RustOS shell
//! depends = fs >=1.0.0
//! location = pkgs/shell-0.1.0.rpk
//! crc32 = a1b2c3d4
//! ```
//!
//! `location` is a path relative to the directory the packages live
//! in; `crc32` is the CRC-32/IEEE of the whole `.rpk` file, checked
//! on install. `description` and `depends` are optional; `name`,
//! `version`, `location` and `crc32` are required.

use crate::manifest::parse_crc32_hex;
use crate::version::{Dependency, Version, VersionReq};
use std::fs;
use std::path::Path;

/// One package listed in an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub version: Version,
    pub description: String,
    pub depends: Vec<Dependency>,
    /// Path to the `.rpk` file, relative to the package directory.
    pub location: String,
    /// CRC-32/IEEE of the whole `.rpk` file.
    pub crc32: u32,
}

impl IndexEntry {
    pub fn serialise(&self) -> String {
        let mut out = String::new();
        out.push_str("[pkg]\n");
        out.push_str(&format!("name = {}\n", self.name));
        out.push_str(&format!("version = {}\n", self.version));
        if !self.description.is_empty() {
            out.push_str(&format!("description = {}\n", self.description));
        }
        if !self.depends.is_empty() {
            let deps: Vec<String> = self.depends.iter().map(|d| d.to_string()).collect();
            out.push_str(&format!("depends = {}\n", deps.join(", ")));
        }
        out.push_str(&format!("location = {}\n", self.location));
        out.push_str(&format!("crc32 = {:08x}\n", self.crc32));
        out
    }
}

/// The package index: an ordered list of [`IndexEntry`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageIndex {
    pub entries: Vec<IndexEntry>,
}

impl PackageIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an entry, replacing any existing entry with the same
    /// name and version. Entries are kept in canonical order
    /// (name, then version), matching [`PackageIndex::serialise`].
    pub fn add(&mut self, entry: IndexEntry) {
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|e| e.name == entry.name && e.version == entry.version)
        {
            *slot = entry;
        } else {
            self.entries.push(entry);
        }
        self.entries
            .sort_by(|a, b| (a.name.clone(), a.version).cmp(&(b.name.clone(), b.version)));
    }

    /// Remove entries. With `version == None` removes every version
    /// of `name`. Returns how many entries were removed.
    pub fn remove(&mut self, name: &str, version: Option<Version>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| {
            !(e.name == name && (version.is_none() || Some(e.version) == version))
        });
        before - self.entries.len()
    }

    /// All versions of `name`, newest first.
    pub fn query(&self, name: &str) -> Vec<&IndexEntry> {
        let mut found: Vec<&IndexEntry> =
            self.entries.iter().filter(|e| e.name == name).collect();
        found.sort_by(|a, b| b.version.cmp(&a.version));
        found
    }

    /// The newest version of `name` satisfying every requirement in
    /// `reqs` (all must match). `None` when nothing qualifies.
    pub fn find_satisfying(&self, name: &str, reqs: &[VersionReq]) -> Option<&IndexEntry> {
        self.query(name)
            .into_iter()
            .find(|e| reqs.iter().all(|r| r.matches(&e.version)))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialise the whole index. Entries are emitted in sorted
    /// order (name, then version) so output is deterministic.
    pub fn serialise(&self) -> String {
        let mut sorted = self.entries.clone();
        sorted.sort_by(|a, b| (a.name.clone(), a.version).cmp(&(b.name.clone(), b.version)));
        let mut out = String::from("# RustOS package index\n");
        for e in &sorted {
            out.push('\n');
            out.push_str(&e.serialise());
        }
        out
    }

    /// Parse index text. Errors (never panics) on malformed input.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut index = PackageIndex::new();
        let mut current: Option<IndexEntry> = None;
        let mut seen: Vec<&str> = Vec::new();
        // Move the in-progress entry into the index, requiring every
        // mandatory key to have been seen in its block.
        fn flush(
            current: &mut Option<IndexEntry>,
            seen: &[&str],
            index: &mut PackageIndex,
        ) -> Result<(), String> {
            if let Some(entry) = current.take() {
                for required in ["name", "version", "location", "crc32"] {
                    if !seen.contains(&required) {
                        return Err(format!(
                            "index entry {:?} is missing required key `{}`",
                            entry.name, required
                        ));
                    }
                }
                index.add(entry);
            }
            Ok(())
        }
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[pkg]" {
                flush(&mut current, &seen, &mut index)?;
                current = Some(IndexEntry {
                    name: String::new(),
                    version: Version::new(0, 0, 0),
                    description: String::new(),
                    depends: Vec::new(),
                    location: String::new(),
                    crc32: 0,
                });
                seen.clear();
                continue;
            }
            let entry = current.as_mut().ok_or_else(|| {
                format!("index line {}: entry outside a [pkg] block: {:?}", lineno + 1, line)
            })?;
            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => {
                    return Err(format!(
                        "index line {}: expected `key = value`, got {:?}",
                        lineno + 1, line
                    ))
                }
            };
            if seen.contains(&key) {
                return Err(format!(
                    "index line {}: duplicate key {:?} in [pkg] block",
                    lineno + 1, key
                ));
            }
            seen.push(key);
            match key {
                "name" => {
                    crate::version::validate_package_name(value).map_err(|e| {
                        format!("index line {}: {}", lineno + 1, e)
                    })?;
                    entry.name = value.to_string();
                }
                "version" => {
                    entry.version = value.parse::<Version>().map_err(|e| {
                        format!("index line {}: {}", lineno + 1, e)
                    })?;
                }
                "description" => entry.description = value.to_string(),
                "depends" => {
                    if !value.is_empty() {
                        for part in value.split(',') {
                            let part = part.trim();
                            if part.is_empty() {
                                return Err(format!(
                                    "index line {}: empty entry in depends list",
                                    lineno + 1
                                ));
                            }
                            entry.depends.push(Dependency::parse(part).map_err(|e| {
                                format!("index line {}: {}", lineno + 1, e)
                            })?);
                        }
                    }
                }
                "location" => {
                    if value.is_empty() {
                        return Err(format!("index line {}: empty location", lineno + 1));
                    }
                    entry.location = value.to_string();
                }
                "crc32" => {
                    entry.crc32 = parse_crc32_hex(value).map_err(|e| {
                        format!("index line {}: {}", lineno + 1, e)
                    })?;
                }
                other => {
                    return Err(format!(
                        "index line {}: unknown key {:?}",
                        lineno + 1, other
                    ))
                }
            }
        }
        flush(&mut current, &seen, &mut index)?;
        // Validate required fields on every entry.
        for e in &index.entries {
            if e.name.is_empty() {
                return Err("index entry is missing required key `name`".to_string());
            }
            if e.location.is_empty() {
                return Err(format!(
                    "index entry {:?} is missing required key `location`",
                    e.name
                ));
            }
        }
        Ok(index)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read index {}: {}", path.display(), e))?;
        Self::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("cannot create index dir {}: {}", parent.display(), e)
                })?;
            }
        }
        fs::write(path, self.serialise())
            .map_err(|e| format!("cannot write index {}: {}", path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionReq;

    fn entry(name: &str, v: (u32, u32, u32)) -> IndexEntry {
        IndexEntry {
            name: name.to_string(),
            version: Version::new(v.0, v.1, v.2),
            description: String::new(),
            depends: Vec::new(),
            location: format!("pkgs/{}-{}.{}.{}.rpk", name, v.0, v.1, v.2),
            crc32: 0x1234_5678,
        }
    }

    #[test]
    fn index_roundtrip() {
        let mut idx = PackageIndex::new();
        let mut shell = entry("shell", (0, 1, 0));
        shell.description = "The shell".to_string();
        shell.depends = vec![Dependency::new(
            "fs",
            VersionReq::GreaterOrEqual(Version::new(1, 0, 0)),
        )];
        idx.add(shell);
        idx.add(entry("fs", (1, 0, 0)));
        idx.add(entry("fs", (1, 1, 0)));
        let text = idx.serialise();
        let back = PackageIndex::parse(&text).unwrap();
        assert_eq!(back, idx);
        assert_eq!(back.serialise(), text);
    }

    #[test]
    fn index_query_and_find() {
        let mut idx = PackageIndex::new();
        idx.add(entry("fs", (1, 0, 0)));
        idx.add(entry("fs", (1, 2, 0)));
        idx.add(entry("fs", (2, 0, 0)));
        let versions: Vec<String> =
            idx.query("fs").iter().map(|e| e.version.to_string()).collect();
        assert_eq!(versions, vec!["2.0.0", "1.2.0", "1.0.0"]);
        assert!(idx.query("nope").is_empty());
        let found = idx
            .find_satisfying("fs", &[VersionReq::parse(">=1.1.0").unwrap()])
            .unwrap();
        assert_eq!(found.version, Version::new(2, 0, 0));
        let found = idx
            .find_satisfying("fs", &[VersionReq::parse("<2.0.0").unwrap()])
            .unwrap();
        assert_eq!(found.version, Version::new(1, 2, 0));
        assert!(idx
            .find_satisfying("fs", &[VersionReq::parse(">=3.0.0").unwrap()])
            .is_none());
        // Multiple requirements must all hold.
        assert!(idx
            .find_satisfying(
                "fs",
                &[
                    VersionReq::parse(">=1.0.0").unwrap(),
                    VersionReq::parse("<1.2.0").unwrap(),
                ]
            )
            .unwrap()
            .version
            == Version::new(1, 0, 0));
    }

    #[test]
    fn index_add_replaces_and_remove_works() {
        let mut idx = PackageIndex::new();
        idx.add(entry("fs", (1, 0, 0)));
        let mut newer = entry("fs", (1, 0, 0));
        newer.crc32 = 0xdead_beef;
        idx.add(newer);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.query("fs")[0].crc32, 0xdead_beef);
        idx.add(entry("fs", (1, 1, 0)));
        assert_eq!(idx.remove("fs", Some(Version::new(1, 0, 0))), 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.remove("fs", None), 1);
        assert!(idx.is_empty());
        assert_eq!(idx.remove("fs", None), 0);
    }

    #[test]
    fn index_rejects_malformed() {
        assert!(PackageIndex::parse("name = fs\nversion = 1.0.0\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nversion = 1.0.0\nlocation = x.rpk\ncrc32 = 0123abcd\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nname = fs\nversion = 1.0.0\ncrc32 = 0123abcd\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nname = fs\nversion = 1.0.0\nlocation = x.rpk\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nname = fs\nversion = 1.0.0\nlocation = x.rpk\ncrc32 = zzzz\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nname = fs\nname = fs2\nversion = 1.0.0\nlocation = x.rpk\ncrc32 = 0123abcd\n").is_err());
        assert!(PackageIndex::parse("[pkg]\nname = fs\nversion = 1.0.0\nlocation = x.rpk\ncrc32 = 0123abcd\nbogus = 1\n").is_err());
        // Empty index parses fine.
        assert!(PackageIndex::parse("# nothing here\n").unwrap().is_empty());
    }
}
