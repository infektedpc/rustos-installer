//! Install, remove and upgrade packages in a target root.
//!
//! A target root is a plain directory on the host (during development
//! it stands in for the RustOS root filesystem; the same layout is
//! what [`crate::system::install_system`] writes into a rustfs image).
//! Package files land at `<root>/<manifest-path>` and the installed
//! database lives at `<root>/.rustos/installed.db`.
//!
//! Install flow: resolve dependencies against the index, then for
//! each package in dependency order verify the `.rpk` file's whole
//! checksum against the index entry, verify the archive itself, and
//! extract it via a staging directory so a bad archive never leaves
//! half-written files in the root. Re-installing a package at the
//! version already installed is a no-op.

use crate::archive::{crc32_ieee, join_manifest_path, read_archive};
use crate::db::{InstalledDb, InstalledPackage};
use crate::index::{IndexEntry, PackageIndex};
use crate::resolver::resolve;
use crate::version::{Dependency, Version, VersionReq};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn staging_dir() -> PathBuf {
    let n = STAGE_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("rustos-installer-stage-{}-{}", std::process::id(), n))
}

/// Install `name` (matching `req`) and its dependencies from
/// `pkg_dir` (where the index's `location` paths are rooted) into
/// `root`. Returns the packages newly installed, as `"name version"`
/// strings in install order. Packages already installed at the
/// resolved version are skipped, not reinstalled.
pub fn install(
    index: &PackageIndex,
    pkg_dir: &Path,
    root: &Path,
    name: &str,
    req: &VersionReq,
) -> Result<Vec<String>, String> {
    let plan = resolve(index, &[Dependency::new(name, req.clone())])?;
    install_plan(index, pkg_dir, root, &plan)
}

/// Install an already-resolved plan (see [`resolve`]).
pub fn install_plan(
    index: &PackageIndex,
    pkg_dir: &Path,
    root: &Path,
    plan: &[IndexEntry],
) -> Result<Vec<String>, String> {
    let _ = index; // The plan already carries everything we need.
    let mut db = InstalledDb::load(root)?;
    let mut installed: Vec<String> = Vec::new();
    for entry in plan {
        if let Some(existing) = db.find(&entry.name) {
            if existing.version == entry.version {
                continue; // Already installed at this version.
            }
        }
        install_one(pkg_dir, root, &mut db, entry)?;
        installed.push(format!("{} {}", entry.name, entry.version));
    }
    db.save(root)?;
    Ok(installed)
}

/// Upgrade `name` to the newest version in the index. Other
/// installed packages are pinned at their current versions, so an
/// upgrade that would force a dependency change fails with a
/// conflict error instead of silently moving other packages.
/// Returns the new version.
pub fn upgrade(
    index: &PackageIndex,
    pkg_dir: &Path,
    root: &Path,
    name: &str,
) -> Result<Version, String> {
    let db = InstalledDb::load(root)?;
    let current = db
        .find(name)
        .ok_or_else(|| format!("package {:?} is not installed", name))?;
    let current_version = current.version;
    let target = index
        .query(name)
        .into_iter()
        .find(|e| e.version > current_version)
        .ok_or_else(|| {
            format!("package {} is already at the newest known version ({})", name, current_version)
        })?;
    let target_version = target.version;
    // Resolve the upgrade plus exact pins for everything else.
    let mut requested = vec![Dependency::new(name, VersionReq::Exact(target_version))];
    for pkg in &db.packages {
        if pkg.name != name {
            requested.push(Dependency::new(&pkg.name, VersionReq::Exact(pkg.version)));
        }
    }
    let plan = resolve(index, &requested)?;
    install_plan(index, pkg_dir, root, &plan)?;
    Ok(target_version)
}

/// Remove `name` from `root`. Refuses when another installed package
/// depends on it, unless `force` is set. Removes the package's files
/// (and prunes directories left empty) and its database record.
pub fn remove(root: &Path, name: &str, force: bool) -> Result<(), String> {
    let mut db = InstalledDb::load(root)?;
    let pkg = db
        .find(name)
        .ok_or_else(|| format!("package {:?} is not installed", name))?
        .clone();
    let dependents = db.dependents_of(name);
    if !dependents.is_empty() && !force {
        return Err(format!(
            "cannot remove {:?}: still needed by {}",
            name,
            dependents.join(", ")
        ));
    }
    for file in &pkg.files {
        let path = join_manifest_path(root, file);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!("cannot remove {}: {}", path.display(), e));
            }
        }
        prune_empty_parents(root, &path);
    }
    db.remove(name);
    db.save(root)?;
    Ok(())
}

/// List installed packages as `(name, version)` pairs, sorted by name.
pub fn list_installed(root: &Path) -> Result<Vec<(String, Version)>, String> {
    let db = InstalledDb::load(root)?;
    let mut out: Vec<(String, Version)> =
        db.packages.iter().map(|p| (p.name.clone(), p.version)).collect();
    out.sort();
    Ok(out)
}

/// Install one resolved package: verify the archive file against the
/// index checksum, verify the archive itself, then copy its files
/// into the root (replacing an older version's files that are no
/// longer shipped) and record it in the database.
fn install_one(
    pkg_dir: &Path,
    root: &Path,
    db: &mut InstalledDb,
    entry: &IndexEntry,
) -> Result<(), String> {
    let archive_path = pkg_dir.join(&entry.location);
    let bytes = fs::read(&archive_path).map_err(|e| {
        format!("cannot read package file {}: {}", archive_path.display(), e)
    })?;
    let file_crc = crc32_ieee(&bytes);
    if file_crc != entry.crc32 {
        return Err(format!(
            "checksum mismatch for {}: index says {:08x}, file is {:08x}",
            archive_path.display(),
            entry.crc32,
            file_crc
        ));
    }
    let (manifest, files) = read_archive(&archive_path)?;
    if manifest.name != entry.name || manifest.version != entry.version {
        return Err(format!(
            "package {} holds {} {}, but the index says {} {}",
            archive_path.display(),
            manifest.name,
            manifest.version,
            entry.name,
            entry.version
        ));
    }
    // Stage first, so a failure above never leaves partial files.
    let stage = staging_dir();
    fs::create_dir_all(&stage)
        .map_err(|e| format!("cannot create staging dir: {}", e))?;
    let result = (|| -> Result<(), String> {
        for (name, data) in &files {
            let dest = join_manifest_path(&stage, name);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create dir {}: {}", parent.display(), e))?;
            }
            fs::write(&dest, data)
                .map_err(|e| format!("cannot stage {}: {}", dest.display(), e))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(e);
    }
    // Remove files the old version owned but the new one does not.
    if let Some(old) = db.find(&entry.name).cloned() {
        for file in &old.files {
            if !manifest.files.iter().any(|f| &f.path == file) {
                let path = join_manifest_path(root, file);
                let _ = fs::remove_file(&path);
                prune_empty_parents(root, &path);
            }
        }
    }
    // Copy staged files into the root.
    for (name, _data) in &files {
        let src = join_manifest_path(&stage, name);
        let dest = join_manifest_path(root, name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create dir {}: {}", parent.display(), e))?;
        }
        fs::copy(&src, &dest)
            .map_err(|e| format!("cannot install {}: {}", dest.display(), e))?;
    }
    let _ = fs::remove_dir_all(&stage);
    db.upsert(InstalledPackage {
        name: manifest.name.clone(),
        version: manifest.version,
        depends: manifest.depends.clone(),
        files: manifest.files.iter().map(|f| f.path.clone()).collect(),
    });
    Ok(())
}

/// Remove empty parent directories of `path`, stopping at `root`
/// (and never removing `root` itself). Best effort: errors ignored.
fn prune_empty_parents(root: &Path, path: &Path) {
    let mut dir = path.parent().map(|p| p.to_path_buf());
    while let Some(d) = dir {
        if d == root {
            break;
        }
        match fs::remove_dir(&d) {
            Ok(()) => dir = d.parent().map(|p| p.to_path_buf()),
            Err(_) => break, // Not empty (or gone): stop climbing.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{pack, MANIFEST_FILE_NAME};
    use crate::index::IndexEntry;
    use crate::testutil::{make_package_dir, tmp_path};
    use crate::version::VersionReq;

    /// Build a real .rpk in `pkg_dir` and return its index entry.
    fn make_indexed_pkg(
        pkg_dir: &Path,
        name: &str,
        version: &str,
        depends: &[(&str, &str)],
        files: &[(&str, &[u8])],
    ) -> IndexEntry {
        let src = make_package_dir(name, version, files);
        // Rewrite the manifest with dependencies when needed.
        if !depends.is_empty() {
            let mut text = fs::read_to_string(src.join(MANIFEST_FILE_NAME)).unwrap();
            let deps: Vec<String> =
                depends.iter().map(|(n, r)| format!("{} {}", n, r)).collect();
            text = text.replace(
                &format!("version = {}\n", version),
                &format!("version = {}\ndepends = {}\n", version, deps.join(", ")),
            );
            fs::write(src.join(MANIFEST_FILE_NAME), text).unwrap();
        }
        let file_name = format!("{}-{}.rpk", name, version);
        let out = pkg_dir.join(&file_name);
        let manifest = pack(&src, &out).unwrap();
        let crc = crc32_ieee(&fs::read(&out).unwrap());
        let _ = fs::remove_dir_all(&src);
        IndexEntry {
            name: manifest.name,
            version: manifest.version,
            description: manifest.description,
            depends: manifest.depends,
            location: file_name,
            crc32: crc,
        }
    }

    fn index_of(entries: Vec<IndexEntry>) -> PackageIndex {
        let mut index = PackageIndex::new();
        for e in entries {
            index.add(e);
        }
        index
    }

    #[test]
    fn install_with_diamond_deps_and_list() {
        let pkg_dir = tmp_path("pkgs-diamond");
        fs::create_dir_all(&pkg_dir).unwrap();
        let core = make_indexed_pkg(&pkg_dir, "core", "1.0.0", &[], &[("lib/core.so", b"core")]);
        let a = make_indexed_pkg(&pkg_dir, "lib-a", "1.0.0", &[("core", ">=1.0.0")], &[("lib/a.so", b"a")]);
        let b = make_indexed_pkg(&pkg_dir, "lib-b", "1.0.0", &[("core", ">=1.0.0")], &[("lib/b.so", b"b")]);
        let app = make_indexed_pkg(
            &pkg_dir, "app", "1.0.0",
            &[("lib-a", "*"), ("lib-b", "*")],
            &[("bin/app", b"app")],
        );
        let index = index_of(vec![core, a, b, app]);
        let root = tmp_path("root-diamond");
        let installed = install(&index, &pkg_dir, &root, "app", &VersionReq::Any).unwrap();
        assert_eq!(installed, vec!["core 1.0.0", "lib-a 1.0.0", "lib-b 1.0.0", "app 1.0.0"]);
        assert_eq!(fs::read(root.join("bin/app")).unwrap(), b"app");
        assert_eq!(fs::read(root.join("lib/a.so")).unwrap(), b"a");
        assert_eq!(fs::read(root.join("lib/core.so")).unwrap(), b"core");
        let listed = list_installed(&root).unwrap();
        assert_eq!(
            listed,
            vec![
                ("app".to_string(), Version::new(1, 0, 0)),
                ("core".to_string(), Version::new(1, 0, 0)),
                ("lib-a".to_string(), Version::new(1, 0, 0)),
                ("lib-b".to_string(), Version::new(1, 0, 0)),
            ]
        );
        // Re-install is a no-op.
        let again = install(&index, &pkg_dir, &root, "app", &VersionReq::Any).unwrap();
        assert!(again.is_empty());
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_refuses_when_depended_on_then_force_works() {
        let pkg_dir = tmp_path("pkgs-remove");
        fs::create_dir_all(&pkg_dir).unwrap();
        let core = make_indexed_pkg(&pkg_dir, "core", "1.0.0", &[], &[("lib/core.so", b"core")]);
        let app = make_indexed_pkg(&pkg_dir, "app", "1.0.0", &[("core", "*")], &[("bin/app", b"app")]);
        let index = index_of(vec![core, app]);
        let root = tmp_path("root-remove");
        install(&index, &pkg_dir, &root, "app", &VersionReq::Any).unwrap();
        let err = remove(&root, "core", false).unwrap_err();
        assert!(err.contains("still needed by"), "{}", err);
        assert!(err.contains("app 1.0.0"), "{}", err);
        assert!(remove(&root, "core", true).is_ok());
        assert!(!root.join("lib/core.so").exists());
        // Removing something not installed is an error.
        assert!(remove(&root, "ghost", false).is_err());
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_prunes_files_and_db() {
        let pkg_dir = tmp_path("pkgs-remove2");
        fs::create_dir_all(&pkg_dir).unwrap();
        let app = make_indexed_pkg(&pkg_dir, "app", "1.0.0", &[], &[("bin/app", b"app")]);
        let index = index_of(vec![app]);
        let root = tmp_path("root-remove2");
        install(&index, &pkg_dir, &root, "app", &VersionReq::Any).unwrap();
        assert!(root.join("bin/app").exists());
        remove(&root, "app", false).unwrap();
        assert!(!root.join("bin/app").exists());
        assert!(!root.join("bin").exists()); // pruned: left empty
        assert!(list_installed(&root).unwrap().is_empty());
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upgrade_bumps_version_and_drops_stale_files() {
        let pkg_dir = tmp_path("pkgs-upgrade");
        fs::create_dir_all(&pkg_dir).unwrap();
        let v1 = make_indexed_pkg(
            &pkg_dir, "app", "1.0.0", &[],
            &[("bin/app", b"v1"), ("bin/old-tool", b"gone in v2")],
        );
        let v2 = make_indexed_pkg(
            &pkg_dir, "app", "1.1.0", &[],
            &[("bin/app", b"v2"), ("bin/new-tool", b"new in v2")],
        );
        let index = index_of(vec![v1, v2]);
        let root = tmp_path("root-upgrade");
        install(&index, &pkg_dir, &root, "app", &VersionReq::Exact(Version::new(1, 0, 0))).unwrap();
        assert_eq!(fs::read(root.join("bin/app")).unwrap(), b"v1");
        let new = upgrade(&index, &pkg_dir, &root, "app").unwrap();
        assert_eq!(new, Version::new(1, 1, 0));
        assert_eq!(fs::read(root.join("bin/app")).unwrap(), b"v2");
        assert_eq!(fs::read(root.join("bin/new-tool")).unwrap(), b"new in v2");
        assert!(!root.join("bin/old-tool").exists());
        // Already newest: error, not a silent no-op.
        assert!(upgrade(&index, &pkg_dir, &root, "app").is_err());
        // Upgrading something not installed is an error.
        assert!(upgrade(&index, &pkg_dir, &root, "ghost").is_err());
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_rejects_index_checksum_mismatch() {
        let pkg_dir = tmp_path("pkgs-badcrc");
        fs::create_dir_all(&pkg_dir).unwrap();
        let mut e = make_indexed_pkg(&pkg_dir, "app", "1.0.0", &[], &[("bin/app", b"app")]);
        e.crc32 ^= 0xFFFF_FFFF; // Lie in the index.
        let index = index_of(vec![e]);
        let root = tmp_path("root-badcrc");
        let err = install(&index, &pkg_dir, &root, "app", &VersionReq::Any).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{}", err);
        assert!(!root.join("bin/app").exists());
        let _ = fs::remove_dir_all(&pkg_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_reports_missing_package_file() {
        let mut index = PackageIndex::new();
        index.add(IndexEntry {
            name: "ghost".to_string(),
            version: Version::new(1, 0, 0),
            description: String::new(),
            depends: vec![],
            location: "ghost-1.0.0.rpk".to_string(),
            crc32: 0,
        });
        let root = tmp_path("root-missing");
        let err = install(&index, &tmp_path("pkgs-missing"), &root, "ghost", &VersionReq::Any)
            .unwrap_err();
        assert!(err.contains("cannot read package"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }
}
