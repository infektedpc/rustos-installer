//! `rustos-installer` CLI: pack, verify, unpack, index, install,
//! remove, upgrade, list, and whole-system install.
//!
//! Argument parsing is hand-rolled (std-only; no clap) and every
//! subcommand prints usage on bad input. All paths are host paths.

use rustos_installer::archive::{crc32_ieee, pack, read_archive, unpack_archive, verify_archive};
use rustos_installer::index::{IndexEntry, PackageIndex};
use rustos_installer::install::{install, list_installed, remove, upgrade};
use rustos_installer::system::{install_system, SystemInstallOptions};
use rustos_installer::version::VersionReq;
use rustos_image::PartitionScheme;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
rustos-installer — RustOS package manager and system installer

USAGE:
  rustos-installer pack <src-dir> <out.rpk>
      Build a .rpk package from a directory holding PKGINFO + files.
  rustos-installer verify <pkg.rpk>
      Verify a package archive (checksums, manifest); print its manifest.
  rustos-installer unpack <pkg.rpk> <dest-dir>
      Verify and extract a package archive.
  rustos-installer index-add <index> <pkg.rpk> [--location <path>]
      Add a package to an index (creates the index if missing).
      --location overrides the stored path (default: the .rpk file name).
  rustos-installer index-list <index>
      List the packages in an index, newest version first per name.
  rustos-installer index-remove <index> <name> [version]
      Remove a package (one version, or all versions) from an index.
  rustos-installer install --index <index> --pkgs <dir> --root <dir> <name> [req]
      Install a package and its dependencies into a target root.
      req is a version requirement, e.g. '>=1.0.0' (default: any).
  rustos-installer remove --root <dir> <name> [--force]
      Remove an installed package (refuses when depended on, unless --force).
  rustos-installer upgrade --index <index> --pkgs <dir> --root <dir> <name>
      Upgrade an installed package to the newest index version.
  rustos-installer list --root <dir>
      List installed packages in a target root.
  rustos-installer install-system --kernel <kernel> --output <disk.img>
      [--bootloader <boot.efi>] --pkgs <a.rpk,b.rpk> [--size-mb 64]
      [--scheme gpt|mbr] [--fs-mb 4]
      Lay a complete RustOS system onto a disk image: GPT/MBR +
      kernel in the ESP, packages in a rustfs image (RUSTFS.IMG).
";

fn fail(msg: &str) -> ExitCode {
    eprintln!("error: {}", msg);
    ExitCode::from(1)
}

/// Tiny flag parser: `--key value` pairs and bare `--flag`s.
/// Returns (positional args, map of flags).
fn parse_flags(args: &[String]) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut positional = Vec::new();
    let mut flags: Vec<(String, Option<String>)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(key) = arg.strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                flags.push((key.to_string(), Some(args[i + 1].clone())));
                i += 2;
            } else {
                flags.push((key.to_string(), None));
                i += 1;
            }
        } else {
            positional.push(arg.clone());
            i += 1;
        }
    }
    (positional, flags)
}

fn flag<'a>(flags: &'a [(String, Option<String>)], key: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_deref())
}

fn has_flag(flags: &[(String, Option<String>)], key: &str) -> bool {
    flags.iter().any(|(k, _)| k == key)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print!("{}", USAGE);
        return ExitCode::SUCCESS;
    }
    let (cmd, rest) = (&args[0], &args[1..]);
    match cmd.as_str() {
        "pack" => cmd_pack(rest),
        "verify" => cmd_verify(rest),
        "unpack" => cmd_unpack(rest),
        "index-add" => cmd_index_add(rest),
        "index-list" => cmd_index_list(rest),
        "index-remove" => cmd_index_remove(rest),
        "install" => cmd_install(rest),
        "remove" => cmd_remove(rest),
        "upgrade" => cmd_upgrade(rest),
        "list" => cmd_list(rest),
        "install-system" => cmd_install_system(rest),
        other => {
            eprintln!("unknown subcommand {:?}\n", other);
            print!("{}", USAGE);
            ExitCode::from(1)
        }
    }
}

fn cmd_pack(args: &[String]) -> ExitCode {
    if args.len() != 2 {
        return fail("usage: rustos-installer pack <src-dir> <out.rpk>");
    }
    match pack(Path::new(&args[0]), Path::new(&args[1])) {
        Ok(manifest) => {
            println!("packed {} {} -> {}", manifest.name, manifest.version, args[1]);
            for f in &manifest.files {
                println!("  {} {:08x}", f.path, f.crc32.unwrap_or(0));
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_verify(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        return fail("usage: rustos-installer verify <pkg.rpk>");
    }
    match verify_archive(Path::new(&args[0])) {
        Ok(manifest) => {
            println!("{}: OK ({} {}, {} file(s))", args[0], manifest.name, manifest.version, manifest.files.len());
            print!("{}", manifest.serialise());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_unpack(args: &[String]) -> ExitCode {
    if args.len() != 2 {
        return fail("usage: rustos-installer unpack <pkg.rpk> <dest-dir>");
    }
    match unpack_archive(Path::new(&args[0]), Path::new(&args[1])) {
        Ok(manifest) => {
            println!("unpacked {} {} -> {}", manifest.name, manifest.version, args[1]);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_index_add(args: &[String]) -> ExitCode {
    let (pos, flags) = parse_flags(args);
    if pos.len() != 2 {
        return fail("usage: rustos-installer index-add <index> <pkg.rpk> [--location <path>]");
    }
    let index_path = Path::new(&pos[0]);
    let pkg_path = Path::new(&pos[1]);
    let (manifest, _files) = match read_archive(pkg_path) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let bytes = match fs::read(pkg_path) {
        Ok(b) => b,
        Err(e) => return fail(&format!("cannot read {}: {}", pkg_path.display(), e)),
    };
    let location = flag(&flags, "location")
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            pkg_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| pos[1].clone())
        });
    // A missing index file means "start a new index".
    let mut index = if index_path.exists() {
        match PackageIndex::load(index_path) {
            Ok(i) => i,
            Err(e) => return fail(&e),
        }
    } else {
        PackageIndex::new()
    };
    index.add(IndexEntry {
        name: manifest.name.clone(),
        version: manifest.version,
        description: manifest.description.clone(),
        depends: manifest.depends.clone(),
        location,
        crc32: crc32_ieee(&bytes),
    });
    match index.save(index_path) {
        Ok(()) => {
            println!("added {} {} to {}", manifest.name, manifest.version, pos[0]);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_index_list(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        return fail("usage: rustos-installer index-list <index>");
    }
    let index = match PackageIndex::load(Path::new(&args[0])) {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    let mut names: Vec<String> = index.entries.iter().map(|e| e.name.clone()).collect();
    names.sort();
    names.dedup();
    for name in names {
        for e in index.query(&name) {
            let deps = if e.depends.is_empty() {
                String::new()
            } else {
                let ds: Vec<String> = e.depends.iter().map(|d| d.to_string()).collect();
                format!(" [{}]", ds.join(", "))
            };
            println!("{} {}  {}{}", e.name, e.version, e.location, deps);
        }
    }
    ExitCode::SUCCESS
}

fn cmd_index_remove(args: &[String]) -> ExitCode {
    if args.len() < 2 || args.len() > 3 {
        return fail("usage: rustos-installer index-remove <index> <name> [version]");
    }
    let index_path = Path::new(&args[0]);
    let mut index = match PackageIndex::load(index_path) {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    let version = if args.len() == 3 {
        match args[2].parse::<rustos_installer::Version>() {
            Ok(v) => Some(v),
            Err(e) => return fail(&e),
        }
    } else {
        None
    };
    let removed = index.remove(&args[1], version);
    if removed == 0 {
        return fail(&format!("no such package {:?} in the index", args[1]));
    }
    match index.save(index_path) {
        Ok(()) => {
            println!("removed {} entr{} from {}", removed, if removed == 1 { "y" } else { "ies" }, args[0]);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn load_index_and_root(args: &[String]) -> Result<(PackageIndex, PathBuf, PathBuf), String> {
    let (_pos, flags) = parse_flags(args);
    let index_path = flag(&flags, "index")
        .ok_or_else(|| "missing --index <index>".to_string())?;
    let pkgs = flag(&flags, "pkgs").ok_or_else(|| "missing --pkgs <dir>".to_string())?;
    let root = flag(&flags, "root").ok_or_else(|| "missing --root <dir>".to_string())?;
    let index = PackageIndex::load(Path::new(index_path))?;
    Ok((index, PathBuf::from(pkgs), PathBuf::from(root)))
}

fn cmd_install(args: &[String]) -> ExitCode {
    let (pos, flags) = parse_flags(args);
    let (index, pkg_dir, root) = match load_index_and_root(args) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let _ = flags;
    if pos.is_empty() || pos.len() > 2 {
        return fail("usage: rustos-installer install --index <index> --pkgs <dir> --root <dir> <name> [req]");
    }
    let req = if pos.len() == 2 {
        match VersionReq::parse(&pos[1]) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        }
    } else {
        VersionReq::Any
    };
    match install(&index, &pkg_dir, &root, &pos[0], &req) {
        Ok(installed) => {
            if installed.is_empty() {
                println!("{} is already installed", pos[0]);
            } else {
                for p in installed {
                    println!("installed {}", p);
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_remove(args: &[String]) -> ExitCode {
    let (pos, flags) = parse_flags(args);
    let root = match flag(&flags, "root") {
        Some(r) => PathBuf::from(r),
        None => return fail("usage: rustos-installer remove --root <dir> <name> [--force]"),
    };
    if pos.len() != 1 {
        return fail("usage: rustos-installer remove --root <dir> <name> [--force]");
    }
    match remove(&root, &pos[0], has_flag(&flags, "force")) {
        Ok(()) => {
            println!("removed {}", pos[0]);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_upgrade(args: &[String]) -> ExitCode {
    let (pos, _flags) = parse_flags(args);
    let (index, pkg_dir, root) = match load_index_and_root(args) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    if pos.len() != 1 {
        return fail("usage: rustos-installer upgrade --index <index> --pkgs <dir> --root <dir> <name>");
    }
    match upgrade(&index, &pkg_dir, &root, &pos[0]) {
        Ok(version) => {
            println!("upgraded {} to {}", pos[0], version);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_list(args: &[String]) -> ExitCode {
    let (pos, flags) = parse_flags(args);
    let root = match flag(&flags, "root") {
        Some(r) => PathBuf::from(r),
        None => return fail("usage: rustos-installer list --root <dir>"),
    };
    if !pos.is_empty() {
        return fail("usage: rustos-installer list --root <dir>");
    }
    match list_installed(&root) {
        Ok(pkgs) => {
            if pkgs.is_empty() {
                println!("(nothing installed in {})", root.display());
            } else {
                for (name, version) in pkgs {
                    println!("{} {}", name, version);
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn cmd_install_system(args: &[String]) -> ExitCode {
    let (_pos, flags) = parse_flags(args);
    let kernel = match flag(&flags, "kernel") {
        Some(k) => PathBuf::from(k),
        None => return fail("usage: rustos-installer install-system --kernel <kernel> --output <disk.img> [--bootloader <boot.efi>] --pkgs <a.rpk,b.rpk> [--size-mb 64] [--scheme gpt|mbr] [--fs-mb 4]"),
    };
    let output = match flag(&flags, "output") {
        Some(o) => PathBuf::from(o),
        None => return fail("missing --output <disk.img>"),
    };
    let size_mb: u64 = match flag(&flags, "size-mb") {
        Some(s) => match s.parse() {
            Ok(v) => v,
            Err(_) => return fail("--size-mb must be a number"),
        },
        None => 64,
    };
    let fs_mb: usize = match flag(&flags, "fs-mb") {
        Some(s) => match s.parse() {
            Ok(v) => v,
            Err(_) => return fail("--fs-mb must be a number"),
        },
        None => 4,
    };
    let scheme = match flag(&flags, "scheme") {
        Some(s) => match PartitionScheme::parse(s) {
            Some(scheme) => scheme,
            None => return fail("--scheme must be gpt or mbr"),
        },
        None => PartitionScheme::Gpt,
    };
    let package_paths: Vec<PathBuf> = match flag(&flags, "pkgs") {
        Some(list) if !list.is_empty() => {
            list.split(',').map(|p| PathBuf::from(p.trim())).collect()
        }
        _ => Vec::new(),
    };
    let opts = SystemInstallOptions {
        kernel_path: kernel,
        bootloader_path: flag(&flags, "bootloader").map(PathBuf::from),
        package_paths,
        output_path: output.clone(),
        size_mb,
        scheme,
        fs_size_mb: fs_mb,
    };
    match install_system(&opts) {
        Ok(report) => {
            println!("wrote {} ({} bytes, {:?})", output.display(), report.image.size_bytes, report.image.scheme);
            println!("ESP files:");
            for f in &report.esp_files {
                println!("  {}", f);
            }
            if report.fs_packages.is_empty() {
                println!("no packages installed into RUSTFS.IMG");
            } else {
                println!("packages in RUSTFS.IMG ({} bytes):", report.fs_image_bytes);
                for p in &report.fs_packages {
                    println!("  {}", p);
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}
