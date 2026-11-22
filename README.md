# rustos-installer

Package manager and system installer for RustOS. Host-side tooling,
std-only, no external crates (it uses path dependencies on the
sibling `rustos-image` and `rustos-fs` crates).

A **package** is a `.rpk` archive: a `PKGINFO` manifest plus files,
each checksummed with CRC-32/IEEE — the same flavour `rustos-image`
uses for GPT headers, so a checksum computed here matches one
computed there. Packages are listed in a **package index**, resolved
with dependency resolution, installed into a target root (a host
directory, or the rustfs image inside a disk image), and tracked in
an **installed-package database** (`<root>/.rustos/installed.db`).

## Package format

### Manifest (`PKGINFO`)

A simple line-based `key = value` format (no TOML crate needed).
`#` starts a comment; blank lines are ignored.

```text
# RustOS package manifest
name = shell
version = 0.1.0
description = The RustOS command shell
depends = fs >=1.0.0, memory =0.1.0
file = bin/shell a1b2c3d4
file = lib/libshell.so 0123abcd
```

* `name` (required): letters, digits, `-`, `_`, starting alphanumerically.
* `version` (required): `major.minor.patch`.
* `description` (optional): one line.
* `depends` (optional): comma-separated `name <req>` entries.
* `file` (zero or more): `<relative-path> <crc32-hex>`. The checksum
  may be omitted in a source directory — `pack` computes it — but a
  present checksum that disagrees with the file bytes is an error.
  Inside a `.rpk` the checksum is always present.

Requirements: `*` (any), `1.2.3` or `=1.2.3` (exact), `>=`, `<=`,
`>`, `<`, and `^1.2.3` (compatible: `>=1.2.3, <2.0.0`;
`^0.2.3` means `<0.3.0`; `^0.0.3` means exactly `0.0.3`).

The parser rejects malformed input instead of guessing: unknown
keys, duplicate scalar keys, bad versions/requirements, absolute
paths or `..` in file paths, duplicate files, bad hex checksums.

### Archive (`.rpk`)

All integers little-endian:

```text
"RPK1"                 magic, 4 bytes
u32 manifest_len
manifest_len bytes     canonical PKGINFO text (UTF-8)
for each file, in manifest order:
  u16 name_len
  name_len bytes       '/'-separated relative path (UTF-8)
  u64 data_len
  u32 crc32            CRC-32/IEEE of the file bytes
  data_len bytes       file bytes
```

`verify`/`unpack` check the magic, the manifest, and every file's
name, length and CRC against both the manifest and the stored
record, and fail closed on any mismatch.

### Index

One `[pkg]` block per package (`#` comments, blank lines ignored):

```text
[pkg]
name = shell
version = 0.1.0
description = The RustOS shell
depends = fs >=1.0.0
location = pkgs/shell-0.1.0.rpk
crc32 = a1b2c3d4
```

`location` is relative to the package directory; `crc32` is the
CRC-32/IEEE of the whole `.rpk` file and is checked on install.

### Installed database

`<root>/.rustos/installed.db`, one `[pkg]` block per installed
package, listing the files it owns and the dependencies it declared
(the dependency list is what lets `remove` refuse when another
installed package still needs this one). A missing file means
"nothing installed yet".

## CLI

```text
rustos-installer pack <src-dir> <out.rpk>
rustos-installer verify <pkg.rpk>
rustos-installer unpack <pkg.rpk> <dest-dir>
rustos-installer index-add <index> <pkg.rpk> [--location <path>]
rustos-installer index-list <index>
rustos-installer index-remove <index> <name> [version]
rustos-installer install --index <index> --pkgs <dir> --root <dir> <name> [req]
rustos-installer remove --root <dir> <name> [--force]
rustos-installer upgrade --index <index> --pkgs <dir> --root <dir> <name>
rustos-installer list --root <dir>
rustos-installer install-system --kernel <kernel> --output <disk.img>
    [--bootloader <boot.efi>] --pkgs <a.rpk,b.rpk> [--size-mb 64]
    [--scheme gpt|mbr] [--fs-mb 4]
```

Example workflow:

```sh
# Build two packages (source dir = PKGINFO + files).
rustos-installer pack ./src-core ./pkgs/core-1.0.0.rpk
rustos-installer pack ./src-shell ./pkgs/shell-0.1.0.rpk

# Register them in an index.
rustos-installer index-add ./index.txt ./pkgs/core-1.0.0.rpk
rustos-installer index-add ./index.txt ./pkgs/shell-0.1.0.rpk

# Install shell (pulls in core) into a target root.
rustos-installer install --index ./index.txt --pkgs ./pkgs --root ./root shell
rustos-installer list --root ./root

# Whole-system install onto a disk image.
rustos-installer install-system --kernel ./kernel.efi \
    --pkgs ./pkgs/core-1.0.0.rpk,./pkgs/shell-0.1.0.rpk \
    --output ./disk.img --size-mb 64 --fs-mb 4
```

## Library API

```rust
use rustos_installer::{Manifest, PackageIndex, Version, VersionReq, Dependency};

// Versions and requirements
let v: Version = "1.2.3".parse().unwrap();
assert!(VersionReq::parse("^1.2.0").unwrap().matches(&v));

// Manifests, archives, index
let m = Manifest::parse("name = shell\nversion = 0.1.0\n")?;
let m = rustos_installer::archive::pack(src_dir, out_rpk)?;
let m = rustos_installer::archive::verify_archive(out_rpk)?;
let index = PackageIndex::load(index_path)?;

// Dependency resolution (topological order, deps first)
let plan = rustos_installer::resolve(&index, &[Dependency::new("shell", VersionReq::Any)])?;

// Install / remove / upgrade into a host root directory
let installed = rustos_installer::install(&index, pkg_dir, root, "shell", &VersionReq::Any)?;
let new_version = rustos_installer::upgrade(&index, pkg_dir, root, "shell")?;
rustos_installer::remove(root, "shell", false)?;
let list = rustos_installer::list_installed(root)?;

// Whole-system install onto a disk image
let report = rustos_installer::install_system(&SystemInstallOptions {
    kernel_path: "kernel.efi".into(),
    package_paths: vec!["pkgs/shell-0.1.0.rpk".into()],
    output_path: "disk.img".into(),
    ..Default::default()
})?;
```

## System installer layout

`install_system` builds the image in three stages, reusing the
sibling crates (nothing here reimplements their formats):

1. `rustos-image::build_image` writes the GPT/MBR layout and the
   kernel into the EFI System Partition (ESP).
2. The packages are installed into a fresh `rustos-fs` image: one
   package per `/pkg/<name>/…`, plus its `PKGINFO` and a root
   `/installed.db` (so the installed database survives into the
   running system).
3. That rustfs image is appended to the ESP as `RUSTFS.IMG` (and an
   optional bootloader as `BOOTX64.EFI`) by the `fat16` module,
   which parses the BPB of the ESP `write_fat16` produced and
   allocates free clusters from the FAT.

The package set for a system install must be dependency-closed:
every dependency has to be satisfied by another package in the set
— there is no index to pull a missing dependency from.

## Known limits

* **No live boot.** QEMU is not installed on this machine, so the
  installed image is verified by inspection only (`inspect_image`,
  reading `RUSTFS.IMG` back out of the ESP and opening it with
  `rustos-fs`). The kernel never runs; nothing here proves the
  installed system boots.
* **No real hardware install.** There is no support for writing to
  a physical disk, partitioning a live device, or a first-boot
  installer UI; the target is always a disk-image file.
* **rustfs limits apply inside the system image:** 48 KiB per file
  (`MAX_FILE_SIZE`), path components at most 56 bytes, 64 entries
  per directory, no file sharing between packages (a file owned by
  two packages is installed twice, once per package; removal
  deletes it even if the other package still lists it).
* **No compression, no signatures.** `.rpk` files are stored
  uncompressed and integrity is CRC-32 only — it detects
  corruption, not tampering. No package signing yet.
* **No network / remote index.** The index and the `.rpk` files
  are local files; there is no download, no repository protocol.
* **Upgrade pins siblings.** `upgrade` pins every other installed
  package at its current version; an upgrade whose new dependencies
  need a newer sibling fails with a conflict instead of moving the
  sibling. (That is deliberate for now; a full solver upgrade is
  future work.)
* **Provisional format choices.** The `PKGINFO` line format, the
  `RPK1` archive layout, the index format and the installed-db
  format are all version 0 and carry no format-version field yet;
  if any of them changes, old files will simply fail to parse.

## Tests

```sh
cargo test   # 43 tests, host-only
```

Covered: version comparison and requirement matching (including
caret), manifest parse/serialise round-trip and malformed-manifest
rejection, CRC-32 known vector and cross-compat with
`rustos_image::crc32_ieee`, pack/unpack round-trip, tampered and
truncated archive rejection, index round-trip/query/remove,
dependency resolution (diamond deps, newest-satisfying pick, cycle
detection, version conflict, missing package), install/remove/
upgrade against temp dirs (including remove-refused-when-depended-
on and stale-file cleanup on upgrade), index-checksum rejection,
FAT16 append/read-back in a real GPT image, and the end-to-end
system install (build a 16 MiB image from a placeholder kernel +
two packages, inspect it back, open the embedded rustfs image and
prove the files landed).
