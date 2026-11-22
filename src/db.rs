//! The installed-package database.
//!
//! Stored as `<root>/.rustos/installed.db`, one `[pkg]` block per
//! installed package, listing the files it owns and the dependencies
//! it declared (the dependency list is what lets `remove` refuse
//! when another installed package still needs this one):
//!
//! ```text
//! [pkg]
//! name = shell
//! version = 0.1.0
//! depends = fs >=1.0.0
//! file = bin/shell
//! file = lib/libshell.so
//! ```
//!
//! A missing database file means "nothing installed yet".

use crate::version::{Dependency, Version};
use std::fs;
use std::path::{Path, PathBuf};

pub const DB_DIR_NAME: &str = ".rustos";
pub const DB_FILE_NAME: &str = "installed.db";

/// One installed package, as recorded in the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    pub version: Version,
    pub depends: Vec<Dependency>,
    /// Files owned by this package, as `/`-separated paths relative
    /// to the install root.
    pub files: Vec<String>,
}

impl InstalledPackage {
    pub fn serialise(&self) -> String {
        let mut out = String::new();
        out.push_str("[pkg]\n");
        out.push_str(&format!("name = {}\n", self.name));
        out.push_str(&format!("version = {}\n", self.version));
        if !self.depends.is_empty() {
            let deps: Vec<String> = self.depends.iter().map(|d| d.to_string()).collect();
            out.push_str(&format!("depends = {}\n", deps.join(", ")));
        }
        for f in &self.files {
            out.push_str(&format!("file = {}\n", f));
        }
        out
    }
}

/// The whole installed database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledDb {
    pub packages: Vec<InstalledPackage>,
}

impl InstalledDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn find(&self, name: &str) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Insert or replace the record for a package (same name).
    /// Records are kept in canonical order (by name), matching
    /// [`InstalledDb::serialise`].
    pub fn upsert(&mut self, pkg: InstalledPackage) {
        if let Some(slot) = self.packages.iter_mut().find(|p| p.name == pkg.name) {
            *slot = pkg;
        } else {
            self.packages.push(pkg);
        }
        self.packages.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Remove the record for `name`. Returns whether one existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| p.name != name);
        before != self.packages.len()
    }

    /// Names of installed packages (other than `name`) that declare
    /// a dependency on `name`.
    pub fn dependents_of(&self, name: &str) -> Vec<String> {
        self.packages
            .iter()
            .filter(|p| p.name != name)
            .filter(|p| p.depends.iter().any(|d| d.name == name))
            .map(|p| format!("{} {}", p.name, p.version))
            .collect()
    }

    pub fn serialise(&self) -> String {
        let mut sorted = self.packages.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = String::from("# RustOS installed-package database\n");
        for p in &sorted {
            out.push('\n');
            out.push_str(&p.serialise());
        }
        out
    }

    /// Parse database text. Like the manifest parser, this rejects
    /// malformed input instead of guessing.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut db = InstalledDb::new();
        let mut current: Option<InstalledPackage> = None;
        let mut seen: Vec<&str> = Vec::new();
        fn flush(
            current: &mut Option<InstalledPackage>,
            seen: &[&str],
            db: &mut InstalledDb,
        ) -> Result<(), String> {
            if let Some(pkg) = current.take() {
                for required in ["name", "version"] {
                    if !seen.contains(&required) {
                        return Err(format!(
                            "installed.db entry {:?} is missing required key `{}`",
                            pkg.name, required
                        ));
                    }
                }
                db.upsert(pkg);
            }
            Ok(())
        }
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[pkg]" {
                flush(&mut current, &seen, &mut db)?;
                current = Some(InstalledPackage {
                    name: String::new(),
                    version: Version::new(0, 0, 0),
                    depends: Vec::new(),
                    files: Vec::new(),
                });
                seen.clear();
                continue;
            }
            let pkg = current.as_mut().ok_or_else(|| {
                format!("installed.db line {}: entry outside a [pkg] block: {:?}", lineno + 1, line)
            })?;
            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => {
                    return Err(format!(
                        "installed.db line {}: expected `key = value`, got {:?}",
                        lineno + 1, line
                    ))
                }
            };
            match key {
                "name" | "version" | "depends" => {
                    if seen.contains(&key) {
                        return Err(format!(
                            "installed.db line {}: duplicate key {:?}",
                            lineno + 1, key
                        ));
                    }
                    seen.push(key);
                    match key {
                        "name" => {
                            crate::version::validate_package_name(value).map_err(|e| {
                                format!("installed.db line {}: {}", lineno + 1, e)
                            })?;
                            pkg.name = value.to_string();
                        }
                        "version" => {
                            pkg.version = value.parse::<Version>().map_err(|e| {
                                format!("installed.db line {}: {}", lineno + 1, e)
                            })?;
                        }
                        "depends" => {
                            if !value.is_empty() {
                                for part in value.split(',') {
                                    let part = part.trim();
                                    if part.is_empty() {
                                        return Err(format!(
                                            "installed.db line {}: empty entry in depends list",
                                            lineno + 1
                                        ));
                                    }
                                    pkg.depends.push(Dependency::parse(part).map_err(|e| {
                                        format!("installed.db line {}: {}", lineno + 1, e)
                                    })?);
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                "file" => {
                    if value.is_empty() {
                        return Err(format!(
                            "installed.db line {}: empty `file` entry",
                            lineno + 1
                        ));
                    }
                    pkg.files.push(value.to_string());
                }
                other => {
                    return Err(format!(
                        "installed.db line {}: unknown key {:?}",
                        lineno + 1, other
                    ))
                }
            }
        }
        flush(&mut current, &seen, &mut db)?;
        Ok(db)
    }

    /// Path of the database file for an install root.
    pub fn path_for(root: &Path) -> PathBuf {
        root.join(DB_DIR_NAME).join(DB_FILE_NAME)
    }

    /// Load the database for an install root; a missing file means
    /// an empty database.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = Self::path_for(root);
        match fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {}", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(format!("cannot read {}: {}", path.display(), e)),
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), String> {
        let path = Self::path_for(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("cannot create db dir {}: {}", parent.display(), e)
            })?;
        }
        fs::write(&path, self.serialise())
            .map_err(|e| format!("cannot write {}: {}", path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionReq;

    #[test]
    fn db_roundtrip() {
        let mut db = InstalledDb::new();
        let mut shell = InstalledPackage {
            name: "shell".to_string(),
            version: Version::new(0, 1, 0),
            depends: vec![Dependency::new(
                "fs",
                VersionReq::GreaterOrEqual(Version::new(1, 0, 0)),
            )],
            files: vec!["bin/shell".to_string(), "lib/libshell.so".to_string()],
        };
        db.upsert(shell.clone());
        db.upsert(InstalledPackage {
            name: "fs".to_string(),
            version: Version::new(1, 0, 0),
            depends: vec![],
            files: vec!["lib/libfs.so".to_string()],
        });
        let text = db.serialise();
        let back = InstalledDb::parse(&text).unwrap();
        assert_eq!(back, db);
        assert_eq!(back.serialise(), text);
        // Dependents: fs is needed by shell.
        assert_eq!(db.dependents_of("fs"), vec!["shell 0.1.0"]);
        assert!(db.dependents_of("shell").is_empty());
        // Upsert replaces the version.
        shell.version = Version::new(0, 2, 0);
        db.upsert(shell);
        assert_eq!(db.find("shell").unwrap().version, Version::new(0, 2, 0));
        assert_eq!(db.packages.len(), 2);
        assert!(db.remove("shell"));
        assert!(!db.remove("shell"));
        assert_eq!(db.packages.len(), 1);
    }

    #[test]
    fn db_rejects_malformed() {
        assert!(InstalledDb::parse("name = a\n").is_err());
        assert!(InstalledDb::parse("[pkg]\nversion = 1.0.0\n").is_err());
        assert!(InstalledDb::parse("[pkg]\nname = a\n").is_err());
        assert!(InstalledDb::parse("[pkg]\nname = a\nversion = 1.0.0\nbogus = 1\n").is_err());
        assert!(InstalledDb::parse("[pkg]\nname = a\nname = b\nversion = 1.0.0\n").is_err());
    }

    #[test]
    fn db_load_missing_is_empty() {
        let root = crate::testutil::tmp_path("no-db-root");
        let db = InstalledDb::load(&root).unwrap();
        assert!(db.packages.is_empty());
    }
}
