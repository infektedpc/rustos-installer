//! Minimal FAT16 read/append support for an existing EFI System
//! Partition inside a disk image.
//!
//! `rustos-image`'s `write_fat16` can only write a single file (the
//! kernel) into a fresh ESP. The system installer needs to add more
//! files afterwards — the rustfs image, an optional bootloader — so
//! this module parses the BPB of an ESP that `write_fat16` produced
//! and appends files to its root directory, allocating free clusters
//! from the FAT. Only the root directory is supported (the ESP
//! layout `rustos-image` produces has a 512-entry root dir and no
//! subdirectories); that is enough for `KERNEL.EFI`, `BOOTX64.EFI`
//! and `RUSTFS.IMG`.
//!
//! All arithmetic is bounds-checked against the image length; any
//! inconsistency is an error, never a panic or an out-of-bounds
//! write.

use rustos_image::SECTOR_SIZE;

/// Geometry of a FAT16 volume, parsed from its boot sector.
#[derive(Debug, Clone, Copy)]
pub struct Fat16Geometry {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u16,
    pub reserved_sectors: u16,
    pub num_fats: u16,
    pub root_entries: u16,
    pub fat_sectors: u16,
}

impl Fat16Geometry {
    pub fn fat_start(&self) -> usize {
        self.reserved_sectors as usize * SECTOR_SIZE
    }
    pub fn root_dir_start(&self) -> usize {
        (self.reserved_sectors as usize
            + self.num_fats as usize * self.fat_sectors as usize)
            * SECTOR_SIZE
    }
    pub fn root_dir_sectors(&self) -> usize {
        (self.root_entries as usize * 32 + SECTOR_SIZE - 1) / SECTOR_SIZE
    }
    pub fn data_start(&self) -> usize {
        self.root_dir_start() + self.root_dir_sectors() * SECTOR_SIZE
    }
    pub fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR_SIZE
    }
    pub fn cluster_offset(&self, cluster: u16) -> usize {
        self.data_start() + (cluster as usize - 2) * self.cluster_bytes()
    }
}

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Parse the FAT16 BPB at the start of the ESP (`esp` is the whole
/// ESP byte range, starting at its first sector).
pub fn parse_geometry(esp: &[u8]) -> Result<Fat16Geometry, String> {
    if esp.len() < SECTOR_SIZE {
        return Err("ESP is smaller than one sector".to_string());
    }
    let bs = &esp[..SECTOR_SIZE];
    if bs[510] != 0x55 || bs[511] != 0xAA {
        return Err("ESP boot sector is missing the 0x55AA signature".to_string());
    }
    let bytes_per_sector = get_u16(bs, 11);
    if bytes_per_sector as usize != SECTOR_SIZE {
        return Err(format!(
            "unsupported bytes-per-sector {} (expected {})",
            bytes_per_sector, SECTOR_SIZE
        ));
    }
    let sectors_per_cluster = bs[13] as u16;
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(format!("bad sectors-per-cluster {}", sectors_per_cluster));
    }
    let reserved_sectors = get_u16(bs, 14);
    let num_fats = bs[16] as u16;
    let root_entries = get_u16(bs, 17);
    let fat_sectors = get_u16(bs, 22);
    if reserved_sectors == 0 || num_fats == 0 || root_entries == 0 || fat_sectors == 0 {
        return Err("ESP BPB has a zero field (not a FAT16 volume)".to_string());
    }
    if &bs[54..62] != b"FAT16   " {
        return Err("ESP is not FAT16 (filesystem type field mismatch)".to_string());
    }
    let geo = Fat16Geometry {
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        root_entries,
        fat_sectors,
    };
    let need = geo.data_start() + geo.cluster_bytes();
    if esp.len() < need {
        return Err("ESP is too small for its own FAT/root directory".to_string());
    }
    Ok(geo)
}

/// Convert a file name to its 8.3 on-disk form, e.g. `RUSTFS.IMG`.
/// Upper-cases, truncates the stem to 8 and the extension to 3
/// characters. Errors when the name has no usable stem.
pub fn fat83_name(name: &str) -> Result<[u8; 11], String> {
    let (stem, ext) = match name.split_once('.') {
        Some((s, e)) => (s, e),
        None => (name, ""),
    };
    if stem.is_empty() {
        return Err(format!("file name {:?} has an empty stem", name));
    }
    if stem.len() > 8 || ext.len() > 3 {
        // Truncation is lossy; refuse rather than silently collide.
        return Err(format!(
            "file name {:?} does not fit 8.3 (stem <= 8, ext <= 3)",
            name
        ));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
        return Err(format!("file name {:?} has characters outside 8.3", name));
    }
    let mut out = [b' '; 11];
    for (i, b) in stem.bytes().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Ok(out)
}

/// Append `data` as a new file `name` (8.3) in the root directory of
/// the ESP that starts at `esp_start_lba` inside `img`. The file must
/// not already exist. Returns the first cluster allocated.
pub fn add_file_to_esp(
    img: &mut [u8],
    esp_start_lba: u64,
    name: &str,
    data: &[u8],
) -> Result<u16, String> {
    let esp_off = esp_start_lba as usize * SECTOR_SIZE;
    if esp_off >= img.len() {
        return Err("ESP start is past the end of the image".to_string());
    }
    let esp_len = img.len() - esp_off;
    let geo = parse_geometry(&img[esp_off..])?;
    let name83 = fat83_name(name)?;

    // Find a free root-directory entry, and refuse duplicates.
    let root_off = esp_off + geo.root_dir_start();
    let mut free_entry: Option<usize> = None;
    for i in 0..geo.root_entries as usize {
        let eoff = root_off + i * 32;
        if eoff + 32 > img.len() {
            return Err("root directory runs past the end of the image".to_string());
        }
        let e = &img[eoff..eoff + 32];
        if e[0] == 0x00 || e[0] == 0xE5 {
            if free_entry.is_none() {
                free_entry = Some(eoff);
            }
            if e[0] == 0x00 {
                break; // No more entries after the first empty one.
            }
            continue;
        }
        if e[0..11] == name83 {
            return Err(format!("file {:?} already exists in the ESP", name));
        }
    }
    let entry_off = free_entry.ok_or_else(|| "ESP root directory is full".to_string())?;

    // How many clusters does the file need? At least one, even empty.
    let clusters_needed =
        ((data.len() + geo.cluster_bytes() - 1) / geo.cluster_bytes()).max(1);
    let total_clusters =
        (esp_len - geo.data_start()) / geo.cluster_bytes();
    if clusters_needed > total_clusters {
        return Err(format!(
            "file ({} bytes, {} clusters) does not fit in the ESP ({} clusters)",
            data.len(),
            clusters_needed,
            total_clusters
        ));
    }
    // Allocate free clusters from the first FAT.
    let fat_off = esp_off + geo.fat_start();
    let fat_bytes = geo.fat_sectors as usize * SECTOR_SIZE;
    if fat_off + fat_bytes > img.len() {
        return Err("FAT runs past the end of the image".to_string());
    }
    let mut clusters: Vec<u16> = Vec::new();
    for cluster in 2..(2 + total_clusters as u16) {
        if clusters.len() == clusters_needed {
            break;
        }
        let slot = fat_off + cluster as usize * 2;
        if get_u16(&img[slot..slot + 2], 0) == 0 {
            clusters.push(cluster);
        }
    }
    if clusters.len() < clusters_needed {
        return Err(format!(
            "ESP is out of free clusters (need {}, found {})",
            clusters_needed,
            clusters.len()
        ));
    }
    // Write the chain into every FAT copy, then the data, then the
    // directory entry (entry last, so a half-written file is never
    // visible).
    for fat_idx in 0..geo.num_fats as usize {
        let base = fat_off + fat_idx * fat_bytes;
        for (i, &cluster) in clusters.iter().enumerate() {
            let next: u16 = if i + 1 == clusters.len() {
                0xFFFF
            } else {
                clusters[i + 1]
            };
            put_u16(&mut img[base + cluster as usize * 2..], 0, next);
        }
    }
    for (i, &cluster) in clusters.iter().enumerate() {
        let start = i * geo.cluster_bytes();
        let end = (start + geo.cluster_bytes()).min(data.len());
        let dst = esp_off + geo.cluster_offset(cluster);
        if dst + geo.cluster_bytes() > img.len() {
            return Err("cluster write would run past the end of the image".to_string());
        }
        img[dst..dst + (end - start)].copy_from_slice(&data[start..end]);
    }
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(&name83);
    entry[11] = 0x20; // archive
    put_u16(&mut entry, 26, clusters[0]);
    put_u32(&mut entry, 28, data.len() as u32);
    img[entry_off..entry_off + 32].copy_from_slice(&entry);
    Ok(clusters[0])
}

/// Read a file back out of the ESP root directory, following its
/// cluster chain. Used by the system-installer tests to prove a file
/// landed; also handy for inspection tooling.
pub fn read_file_from_esp(
    img: &[u8],
    esp_start_lba: u64,
    name: &str,
) -> Result<Vec<u8>, String> {
    let esp_off = esp_start_lba as usize * SECTOR_SIZE;
    if esp_off >= img.len() {
        return Err("ESP start is past the end of the image".to_string());
    }
    let geo = parse_geometry(&img[esp_off..])?;
    let name83 = fat83_name(name)?;
    let root_off = esp_off + geo.root_dir_start();
    let mut found: Option<(u16, u32)> = None;
    for i in 0..geo.root_entries as usize {
        let eoff = root_off + i * 32;
        if eoff + 32 > img.len() {
            break;
        }
        let e = &img[eoff..eoff + 32];
        if e[0] == 0x00 {
            break;
        }
        if e[0] == 0xE5 || e[11] == 0x0F {
            continue;
        }
        if e[0..11] == name83 {
            found = Some((get_u16(e, 26), get_u32(e, 28)));
            break;
        }
    }
    let (mut cluster, size) = found
        .ok_or_else(|| format!("file {:?} not found in the ESP", name))?;
    let fat_off = esp_off + geo.fat_start();
    let mut out = Vec::new();
    let mut guard = 0usize;
    loop {
        if cluster < 2 {
            return Err(format!("bad cluster {} in chain for {:?}", cluster, name));
        }
        let src = esp_off + geo.cluster_offset(cluster);
        if src + geo.cluster_bytes() > img.len() {
            return Err("cluster read would run past the end of the image".to_string());
        }
        let take = geo.cluster_bytes().min(size as usize - out.len());
        out.extend_from_slice(&img[src..src + take]);
        if out.len() >= size as usize {
            break;
        }
        let slot = fat_off + cluster as usize * 2;
        cluster = get_u16(&img[slot..slot + 2], 0);
        if cluster >= 0xFFF8 {
            break;
        }
        guard += 1;
        if guard > 1_000_000 {
            return Err(format!("cluster chain for {:?} is too long (loop?)", name));
        }
    }
    out.truncate(size as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmp_path;
    use rustos_image::{build_image, ImageOptions, PartitionScheme};
    use std::fs;

    fn build_kernel_image() -> (std::path::PathBuf, u64) {
        let kernel = tmp_path("fat-kernel.efi");
        fs::write(&kernel, b"\x7FELF fake kernel bytes").unwrap();
        let out = tmp_path("fat-disk.img");
        let info = build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap();
        let _ = fs::remove_file(&kernel);
        (out, info.esp_start_lba)
    }

    #[test]
    fn fat83_names() {
        assert_eq!(fat83_name("KERNEL.EFI").unwrap(), *b"KERNEL  EFI");
        assert_eq!(fat83_name("RUSTFS.IMG").unwrap(), *b"RUSTFS  IMG");
        assert_eq!(fat83_name("README").unwrap(), *b"README     ");
        assert!(fat83_name("toolongname.efi").is_err());
        assert!(fat83_name("a.longext").is_err());
        assert!(fat83_name(".efi").is_err());
        assert!(fat83_name("a b.efi").is_err());
    }

    #[test]
    fn add_and_read_back_files() {
        let (img_path, esp_start) = build_kernel_image();
        let mut img = fs::read(&img_path).unwrap();
        // Kernel is already there from build_image.
        assert_eq!(
            read_file_from_esp(&img, esp_start, "KERNEL.EFI").unwrap(),
            b"\x7FELF fake kernel bytes"
        );
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        add_file_to_esp(&mut img, esp_start, "RUSTFS.IMG", &payload).unwrap();
        add_file_to_esp(&mut img, esp_start, "BOOTX64.EFI", b"bootloader").unwrap();
        assert_eq!(read_file_from_esp(&img, esp_start, "RUSTFS.IMG").unwrap(), payload);
        assert_eq!(
            read_file_from_esp(&img, esp_start, "BOOTX64.EFI").unwrap(),
            b"bootloader"
        );
        // Kernel untouched.
        assert_eq!(
            read_file_from_esp(&img, esp_start, "KERNEL.EFI").unwrap(),
            b"\x7FELF fake kernel bytes"
        );
        // Duplicate add is refused.
        assert!(add_file_to_esp(&mut img, esp_start, "RUSTFS.IMG", b"x").is_err());
        // Missing file errors.
        assert!(read_file_from_esp(&img, esp_start, "NOPE.TXT").is_err());
        // inspect_image still sees all three files.
        fs::write(&img_path, &img).unwrap();
        let rep = rustos_image::inspect_image(&img_path).unwrap();
        assert_eq!(rep.esp_files.len(), 3);
        assert!(rep.esp_files.iter().any(|f| f.starts_with("KERNEL.EFI")));
        assert!(rep.esp_files.iter().any(|f| f.starts_with("RUSTFS.IMG")));
        assert!(rep.esp_files.iter().any(|f| f.starts_with("BOOTX64.EFI")));
        let _ = fs::remove_file(&img_path);
    }

    #[test]
    fn add_file_rejects_bad_inputs() {
        let (img_path, esp_start) = build_kernel_image();
        let mut img = fs::read(&img_path).unwrap();
        assert!(add_file_to_esp(&mut img, esp_start, "TOOLONGNAME.EFI", b"x").is_err());
        // A file bigger than the whole ESP cannot fit.
        let huge = vec![0u8; 64 * 1024 * 1024];
        assert!(add_file_to_esp(&mut img, esp_start, "BIG.BIN", &huge).is_err());
        // Garbage is not a FAT16 volume.
        let mut garbage = vec![0u8; 4096];
        assert!(parse_geometry(&garbage).is_err());
        garbage[510] = 0x55;
        garbage[511] = 0xAA;
        assert!(parse_geometry(&garbage).is_err());
        let _ = fs::remove_file(&img_path);
    }
}
