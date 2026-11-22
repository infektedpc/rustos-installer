//! rustos-installer: package manager and system installer for RustOS.
//!
//! Host-side tooling (std only). A **package** is a `.rpk` archive:
//! a `PKGINFO` manifest plus files, each checksummed with CRC-32/IEEE
//! (the same flavour `rustos-image` uses for GPT). Packages are
//! listed in a **package index**, resolved with dependency
//! resolution, installed into a target root (a host directory, or
//! the rustfs image inside a disk image), and tracked in an
//! **installed-package database**.
//!
//! Modules:
//!
//! * [`version`] — `major.minor.patch` versions and requirements.
//! * [`manifest`] — the line-based `PKGINFO` manifest format.
//! * [`archive`] — the `.rpk` archive: pack, verify, unpack.
//! * [`index`] — the package index file.
//! * [`resolver`] — dependency resolution (topological order, cycle
//!   and conflict detection).
//! * [`db`] — the installed-package database.
//! * [`install`] — install / remove / upgrade into a target root.
//! * [`fat16`] — append/read files in an existing FAT16 ESP.
//! * [`system`] — lay a whole RustOS system onto a disk image.
//!
//! Nothing in this crate boots: there is no QEMU on this machine,
//! so system images are verified by inspection only (see README).

pub mod archive;
pub mod db;
pub mod fat16;
pub mod index;
pub mod install;
pub mod manifest;
pub mod resolver;
pub mod system;
pub mod version;

pub use archive::crc32_ieee;
pub use db::{InstalledDb, InstalledPackage};
pub use index::{IndexEntry, PackageIndex};
pub use install::{install, list_installed, remove, upgrade};
pub use manifest::{Manifest, ManifestFile};
pub use resolver::resolve;
pub use system::{install_system, SystemInstallOptions, SystemInstallReport};
pub use version::{Dependency, Version, VersionReq};

/// Shared helpers for the crate's own tests: unique temp paths and
/// building small package source directories.
#[cfg(test)]
pub mod testutil {
    use crate::archive::MANIFEST_FILE_NAME;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique path under the system temp dir (no directory is
    /// created; callers create what they need).
    pub fn tmp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("rustos-installer-test-{}-{}-{}", std::process::id(), n, name))
    }

    /// Build a package *source* directory: a `PKGINFO` manifest
    /// (checksums omitted — `pack` fills them in) plus `files` as
    /// `(relative-path, bytes)`. Returns the directory path.
    pub fn make_package_dir(name: &str, version: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = tmp_path(&format!("pkg-{}-{}", name, version));
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = format!("name = {}\nversion = {}\n", name, version);
        for (path, _) in files {
            manifest.push_str(&format!("file = {}\n", path));
        }
        fs::write(dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
        for (path, data) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, data).unwrap();
        }
        dir
    }
}
