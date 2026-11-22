//! The system installer: lay a complete RustOS system onto a disk
//! image.
//!
//! The image is built in three stages, reusing the sibling crates
//! (nothing here reimplements their formats):
//!
//! 1. [`rustos_image::build_image`] writes the GPT/MBR layout and
//!    the kernel into the EFI System Partition (ESP).
//! 2. The requested packages are installed into a fresh
//!    [`rustos_fs::Filesystem`] image (one package per
//!    `/pkg/<name>/…`, plus its `PKGINFO` and a root `/installed.db`),
//!    using the same install rules as a host root.
//! 3. That rustfs image is appended to the ESP as `RUSTFS.IMG`
//!    (and an optional bootloader as `BOOTX64.EFI`) with
//!    [`crate::fat16::add_file_to_esp`].
//!
//! Limits (see README): package files must fit rustfs
//! (`MAX_FILE_SIZE` is 48 KiB per file, path components at most 56
//! bytes); the ESP holds only the kernel, the bootloader and the
//! rustfs image — packages are *not* also unpacked into the ESP.
//! Nothing here boots: there is no QEMU on this machine, so the
//! result is verified by inspection only.

use crate::archive::read_archive;
use crate::db::{InstalledDb, InstalledPackage};
use crate::fat16::add_file_to_esp;
use crate::manifest::Manifest;
use crate::version::Dependency;
use rustos_fs::{Filesystem, MAX_FILE_SIZE, MAX_NAME_LEN};
use rustos_image::{build_image, inspect_image, ImageInfo, ImageOptions, PartitionScheme};
use std::fs;
use std::path::PathBuf;

/// Options for [`install_system`].
#[derive(Debug, Clone)]
pub struct SystemInstallOptions {
    /// Kernel file (`.efi` → `KERNEL.EFI`, anything else → `KERNEL.BIN`).
    pub kernel_path: PathBuf,
    /// Optional bootloader file, placed in the ESP as `BOOTX64.EFI`.
    pub bootloader_path: Option<PathBuf>,
    /// `.rpk` package files to install into the system image.
    pub package_paths: Vec<PathBuf>,
    /// Where to write the finished disk image.
    pub output_path: PathBuf,
    /// Disk image size in MiB (minimum 16).
    pub size_mb: u64,
    /// GPT (default) or MBR partition scheme.
    pub scheme: PartitionScheme,
    /// Size of the rustfs image inside the ESP, in MiB.
    pub fs_size_mb: usize,
}

impl Default for SystemInstallOptions {
    fn default() -> Self {
        Self {
            kernel_path: PathBuf::from("kernel.efi"),
            bootloader_path: None,
            package_paths: Vec::new(),
            output_path: PathBuf::from("disk.img"),
            size_mb: 64,
            scheme: PartitionScheme::Gpt,
            fs_size_mb: 4,
        }
    }
}

/// What [`install_system`] produced.
#[derive(Debug, Clone)]
pub struct SystemInstallReport {
    /// Layout info from `rustos-image`.
    pub image: ImageInfo,
    /// Files in the ESP afterwards (as listed by `inspect_image`).
    pub esp_files: Vec<String>,
    /// Packages installed into the rustfs image, as `"name version"`.
    pub fs_packages: Vec<String>,
    /// Size of the rustfs image (`RUSTFS.IMG`) in bytes.
    pub fs_image_bytes: usize,
}

/// Build a complete RustOS disk image: partition + kernel +
/// packages. Returns a report describing the finished image.
pub fn install_system(opts: &SystemInstallOptions) -> Result<SystemInstallReport, String> {
    // 1. Read and verify every package, and check the set is
    //    self-consistent: every dependency must be satisfied by
    //    another package in the set (the system image is closed —
    //    there is no index to pull a missing dependency from).
    let mut packages: Vec<(Manifest, Vec<(String, Vec<u8>)>)> = Vec::new();
    for path in &opts.package_paths {
        packages.push(read_archive(path)?);
    }
    let mut seen_names: Vec<String> = Vec::new();
    for (manifest, _) in &packages {
        if seen_names.contains(&manifest.name) {
            return Err(format!(
                "duplicate package {:?} in system install",
                manifest.name
            ));
        }
        seen_names.push(manifest.name.clone());
    }
    let manifests: Vec<Manifest> = packages.iter().map(|(m, _)| m.clone()).collect();
    check_set_closed(&manifests)?;

    // 2. Build the rustfs image with the packages under /pkg/<name>/.
    let mut fs_image = Filesystem::create_mb(opts.fs_size_mb)
        .map_err(|e| format!("cannot create rustfs image: {}", e))?;
    // The package tree exists even when no packages are installed,
    // so /pkg always lists (possibly empty).
    fs_image
        .mkdir("/pkg")
        .map_err(|e| format!("cannot create /pkg in rustfs image: {}", e))?;
    let mut db = InstalledDb::new();
    let mut fs_packages: Vec<String> = Vec::new();
    for (manifest, files) in &packages {
        let base = format!("/pkg/{}", manifest.name);
        if manifest.name.len() > MAX_NAME_LEN {
            return Err(format!(
                "package name {:?} exceeds the rustfs name limit of {} bytes",
                manifest.name, MAX_NAME_LEN
            ));
        }
        for (rel, data) in files {
            if data.len() > MAX_FILE_SIZE {
                return Err(format!(
                    "file {:?} of package {} is {} bytes, over the rustfs per-file limit of {}",
                    rel,
                    manifest.name,
                    data.len(),
                    MAX_FILE_SIZE
                ));
            }
            let dest = format!("{}/{}", base, rel);
            fs_image
                .write_file(&dest, data)
                .map_err(|e| format!("cannot write {} into rustfs image: {}", dest, e))?;
        }
        fs_image
            .write_file(
                &format!("{}/PKGINFO", base),
                manifest.serialise().as_bytes(),
            )
            .map_err(|e| format!("cannot write PKGINFO for {}: {}", manifest.name, e))?;
        db.upsert(InstalledPackage {
            name: manifest.name.clone(),
            version: manifest.version,
            depends: manifest.depends.clone(),
            files: manifest.files.iter().map(|f| f.path.clone()).collect(),
        });
        fs_packages.push(format!("{} {}", manifest.name, manifest.version));
    }
    fs_image
        .write_file("/installed.db", db.serialise().as_bytes())
        .map_err(|e| format!("cannot write /installed.db into rustfs image: {}", e))?;
    let fs_bytes = fs_image.into_image();
    let fs_image_bytes = fs_bytes.len();

    // 3. Build the disk image (GPT/MBR + kernel in the ESP).
    let info = build_image(&ImageOptions {
        kernel_path: opts.kernel_path.clone(),
        output_path: opts.output_path.clone(),
        size_mb: opts.size_mb,
        scheme: opts.scheme,
    })?;

    // 4. Append the rustfs image (and bootloader, if given) to the ESP.
    let mut img = fs::read(&opts.output_path)
        .map_err(|e| format!("cannot re-read {}: {}", opts.output_path.display(), e))?;
    add_file_to_esp(&mut img, info.esp_start_lba, "RUSTFS.IMG", &fs_bytes)?;
    if let Some(bootloader) = &opts.bootloader_path {
        let bytes = fs::read(bootloader).map_err(|e| {
            format!("cannot read bootloader {}: {}", bootloader.display(), e)
        })?;
        add_file_to_esp(&mut img, info.esp_start_lba, "BOOTX64.EFI", &bytes)?;
    }
    fs::write(&opts.output_path, &img)
        .map_err(|e| format!("cannot write {}: {}", opts.output_path.display(), e))?;

    let report = inspect_image(&opts.output_path)?;
    Ok(SystemInstallReport {
        image: info,
        esp_files: report.esp_files,
        fs_packages,
        fs_image_bytes,
    })
}

/// Check whether a set of package manifests is dependency-closed
/// (every dependency satisfied inside the set). Split out for reuse
/// and testing; [`install_system`] applies the same rule.
pub fn check_set_closed(manifests: &[Manifest]) -> Result<(), String> {
    for manifest in manifests {
        for dep in &manifest.depends {
            let ok = manifests
                .iter()
                .any(|m| m.name == dep.name && dep.req.matches(&m.version));
            if !ok {
                return Err(format!(
                    "package {} {} needs {} {}, which is not in the set",
                    manifest.name, manifest.version, dep.name, dep.req
                ));
            }
        }
    }
    Ok(())
}

/// Convenience: build the `Dependency` list a package set would need
/// from the outside (dependencies not satisfied inside the set).
pub fn external_dependencies(manifests: &[Manifest]) -> Vec<Dependency> {
    let mut out: Vec<Dependency> = Vec::new();
    for manifest in manifests {
        for dep in &manifest.depends {
            let satisfied = manifests
                .iter()
                .any(|m| m.name == dep.name && dep.req.matches(&m.version));
            if !satisfied && !out.iter().any(|d: &Dependency| d.name == dep.name) {
                out.push(dep.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::pack;
    use crate::fat16::read_file_from_esp;
    use crate::testutil::{make_package_dir, tmp_path};
    use crate::version::VersionReq;
    use rustos_fs::Filesystem;
    use std::path::Path;

    fn package_file(
        dir: &Path,
        name: &str,
        version: &str,
        depends: &[(&str, &str)],
        files: &[(&str, &[u8])],
    ) -> PathBuf {
        let src = make_package_dir(name, version, files);
        if !depends.is_empty() {
            let mut text = fs::read_to_string(src.join("PKGINFO")).unwrap();
            let deps: Vec<String> =
                depends.iter().map(|(n, r)| format!("{} {}", n, r)).collect();
            text = text.replace(
                &format!("version = {}\n", version),
                &format!("version = {}\ndepends = {}\n", version, deps.join(", ")),
            );
            fs::write(src.join("PKGINFO"), text).unwrap();
        }
        let out = dir.join(format!("{}-{}.rpk", name, version));
        pack(&src, &out).unwrap();
        let _ = fs::remove_dir_all(&src);
        out
    }

    #[test]
    fn system_install_end_to_end() {
        let work = tmp_path("sys-e2e");
        fs::create_dir_all(&work).unwrap();
        // Placeholder kernel + optional bootloader.
        let kernel = work.join("kernel.efi");
        fs::write(&kernel, b"\x7FELF placeholder kernel").unwrap();
        let bootloader = work.join("boot.efi");
        fs::write(&bootloader, b"fake bootloader").unwrap();
        // Two packages: shell depends on core.
        let core = package_file(&work, "core", "1.0.0", &[], &[("lib/core.so", b"core-bytes")]);
        let shell = package_file(
            &work, "shell", "0.1.0",
            &[("core", ">=1.0.0")],
            &[("bin/shell", b"shell-bytes"), ("etc/motd.txt", b"welcome to rustos")],
        );
        let output = work.join("disk.img");
        let report = install_system(&SystemInstallOptions {
            kernel_path: kernel.clone(),
            bootloader_path: Some(bootloader.clone()),
            package_paths: vec![core, shell],
            output_path: output.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
            fs_size_mb: 1,
        })
        .unwrap();
        assert_eq!(report.image.size_bytes, 16 * 1024 * 1024);
        assert_eq!(report.fs_packages, vec!["core 1.0.0", "shell 0.1.0"]);
        assert_eq!(report.fs_image_bytes, 1024 * 1024);
        // ESP holds kernel + rustfs image + bootloader.
        assert_eq!(report.esp_files.len(), 3);
        assert!(report.esp_files.iter().any(|f| f.starts_with("KERNEL.EFI")));
        assert!(report.esp_files.iter().any(|f| f.starts_with("RUSTFS.IMG")));
        assert!(report.esp_files.iter().any(|f| f.starts_with("BOOTX64.EFI")));
        // The rustfs image inside the ESP opens and holds the packages.
        let img = fs::read(&output).unwrap();
        let fs_bytes =
            read_file_from_esp(&img, report.image.esp_start_lba, "RUSTFS.IMG").unwrap();
        let fs = Filesystem::open(fs_bytes).unwrap();
        let top: Vec<String> =
            fs.list_dir("/pkg").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(top, vec!["core", "shell"]);
        assert_eq!(fs.read_file("/pkg/shell/bin/shell").unwrap(), b"shell-bytes");
        assert_eq!(fs.read_file("/pkg/shell/etc/motd.txt").unwrap(), b"welcome to rustos");
        assert_eq!(fs.read_file("/pkg/core/lib/core.so").unwrap(), b"core-bytes");
        // PKGINFO and the installed DB round-trip through the image.
        let pkginfo = String::from_utf8(fs.read_file("/pkg/shell/PKGINFO").unwrap()).unwrap();
        assert!(pkginfo.contains("name = shell"), "{}", pkginfo);
        assert!(pkginfo.contains("depends = core >=1.0.0"), "{}", pkginfo);
        let db_text = String::from_utf8(fs.read_file("/installed.db").unwrap()).unwrap();
        let db = InstalledDb::parse(&db_text).unwrap();
        assert_eq!(db.packages.len(), 2);
        assert_eq!(db.find("shell").unwrap().version, crate::version::Version::new(0, 1, 0));
        // The image still inspects cleanly as a GPT image.
        let rep = inspect_image(&output).unwrap();
        assert_eq!(rep.scheme, PartitionScheme::Gpt);
        assert_eq!(rep.partitions.len(), 1);
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn system_install_rejects_unsatisfied_dep() {
        let work = tmp_path("sys-baddep");
        fs::create_dir_all(&work).unwrap();
        let kernel = work.join("kernel.efi");
        fs::write(&kernel, b"\x7FELF kernel").unwrap();
        // shell needs core, but core is not in the set.
        let shell = package_file(
            &work, "shell", "0.1.0",
            &[("core", ">=1.0.0")],
            &[("bin/shell", b"x")],
        );
        let output = work.join("disk.img");
        let err = install_system(&SystemInstallOptions {
            kernel_path: kernel.clone(),
            bootloader_path: None,
            package_paths: vec![shell],
            output_path: output.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
            fs_size_mb: 1,
        })
        .unwrap_err();
        assert!(err.contains("core"), "{}", err);
        assert!(!output.exists());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn system_install_without_packages_still_builds() {
        let work = tmp_path("sys-nopkg");
        fs::create_dir_all(&work).unwrap();
        let kernel = work.join("kernel.bin");
        fs::write(&kernel, b"kernel bytes").unwrap();
        let output = work.join("disk.img");
        let report = install_system(&SystemInstallOptions {
            kernel_path: kernel.clone(),
            bootloader_path: None,
            package_paths: vec![],
            output_path: output.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Mbr,
            fs_size_mb: 1,
        })
        .unwrap();
        assert!(report.fs_packages.is_empty());
        assert_eq!(report.esp_files.len(), 2); // KERNEL.BIN + RUSTFS.IMG
        let img = fs::read(&output).unwrap();
        let fs_bytes =
            read_file_from_esp(&img, report.image.esp_start_lba, "RUSTFS.IMG").unwrap();
        let fs = Filesystem::open(fs_bytes).unwrap();
        assert!(fs.list_dir("/pkg").unwrap().is_empty());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn check_set_closed_and_external_deps() {
        let mut a = Manifest::new("a", crate::version::Version::new(1, 0, 0));
        a.depends = vec![Dependency::new(
            "b",
            VersionReq::GreaterOrEqual(crate::version::Version::new(1, 0, 0)),
        )];
        let b = Manifest::new("b", crate::version::Version::new(1, 0, 0));
        assert!(check_set_closed(&[a.clone()]).is_err());
        assert!(check_set_closed(&[a.clone(), b]).is_ok());
        assert_eq!(external_dependencies(&[a.clone()]).len(), 1);
        assert_eq!(external_dependencies(&[a, Manifest::new("b", crate::version::Version::new(1, 0, 0))]).len(), 0);
    }
}
