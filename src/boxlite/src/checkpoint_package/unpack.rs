use super::*;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use std::io::BufReader;
use std::path::PathBuf;

/// Verified private artifacts. Keep this owner alive and its files unchanged
/// while using the paths; dropping it removes the entire staging directory.
#[derive(Debug)]
pub struct VerifiedPackage {
    staging: tempfile::TempDir,
    manifest: Manifest,
}

impl VerifiedPackage {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn pair_directory(&self) -> PathBuf {
        self.staging.path().join("pair")
    }

    pub fn boot_disk(&self) -> PathBuf {
        self.staging.path().join(BOOT)
    }
}

/// Decode from file offset zero with explicit byte limits. This blocking API
/// checks integrity and internal consistency, not volume authority or VM support.
pub fn unpack(mut archive: File, limits: Limits) -> BoxliteResult<VerifiedPackage> {
    archive.rewind().map_err(invalid)?;
    let staging = tempfile::Builder::new()
        .prefix(".boxlite-checkpoint-package-")
        .tempdir()
        .map_err(invalid)?;
    unpack_staged(archive, limits, staging)
}

fn unpack_staged(
    archive: impl Read,
    limits: Limits,
    staging: tempfile::TempDir,
) -> BoxliteResult<VerifiedPackage> {
    let mut compressed = BufReader::with_capacity(
        64 << 10,
        BoundedReader {
            inner: archive,
            remaining: limits.compressed_bytes,
        },
    );
    // Skippable frames are valid zstd input but are not package frames.
    let mut magic = [0; 4];
    compressed.read_exact(&mut magic).map_err(invalid)?;
    if magic != [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(invalid("package requires an ordinary zstd frame"));
    }
    let mut decoder =
        zstd::stream::read::Decoder::with_buffer(io::Cursor::new(magic).chain(compressed))
            .map_err(invalid)?
            .single_frame();
    decoder.window_log_max(26).map_err(invalid)?;
    let mut expanded = BoundedReader {
        inner: decoder,
        remaining: limits.expanded_bytes,
    };
    let pair_directory = staging.path().join("pair");
    std::fs::create_dir(&pair_directory).map_err(invalid)?;
    let mut seen = BTreeSet::new();
    let mut json = Vec::new();
    for _ in 0..4 {
        let mut header = tar::Header::new_ustar();
        expanded
            .read_exact(header.as_mut_bytes())
            .map_err(invalid)?;
        let bytes = header.as_bytes();
        let checksum: u32 = bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    32
                } else {
                    u32::from(*byte)
                }
            })
            .sum();
        if header.as_ustar().is_none()
            || header.entry_type() != tar::EntryType::Regular
            || header.cksum().map_err(invalid)? != checksum
            || bytes[157..257]
                .iter()
                .chain(&bytes[345..500])
                .any(|&byte| byte != 0)
        {
            return Err(invalid("invalid package USTAR regular-file header"));
        }
        let (name, limit) = [
            (MANIFEST, MANIFEST_LIMIT),
            (MEMORY, limits.memory_bytes),
            (DISK, limits.disk_bytes),
            (BOOT, limits.boot_bytes),
        ]
        .into_iter()
        .find(|(name, _)| {
            bytes[..name.len()] == *name.as_bytes()
                && bytes[name.len()..100].iter().all(|&byte| byte == 0)
        })
        .ok_or_else(|| invalid("unknown package member name"))?;
        if !seen.insert(name) {
            return Err(invalid("duplicate package member"));
        }
        // tar's extended parser discards these high bytes. Check them first
        // so negative or >u64 values cannot masquerade as small lengths.
        if bytes[124] & 0x80 != 0 && bytes[124..128] != [0x80, 0, 0, 0] {
            return Err(invalid(
                "package member length overflows u64 or is negative",
            ));
        }
        let length = header.size().map_err(invalid)?;
        let padding = (512 - length % 512) % 512;
        if length > limit
            || length
                .checked_add(padding)
                .is_none_or(|n| n > expanded.remaining)
        {
            return Err(invalid("package member exceeds byte limit"));
        }
        if name == MANIFEST {
            copy_exact(&mut expanded, &mut json, length).map_err(invalid)?;
        } else {
            let path = if name == BOOT {
                staging.path().join(BOOT)
            } else {
                pair_directory.join(name)
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(invalid)?;
            copy_exact(&mut expanded, &mut file, length).map_err(invalid)?;
            file.sync_all().map_err(invalid)?;
        }
        let mut zeros = [0; 512];
        expanded
            .read_exact(&mut zeros[..padding as usize])
            .map_err(invalid)?;
        if zeros.iter().any(|&byte| byte != 0) {
            return Err(invalid("nonzero package member padding"));
        }
    }
    let mut footer = [0; 1024];
    expanded.read_exact(&mut footer).map_err(invalid)?;
    if footer.iter().any(|&byte| byte != 0) {
        return Err(invalid("package requires exactly two zero footer blocks"));
    }
    require_eof(&mut expanded).map_err(invalid)?;
    require_eof(&mut expanded.inner.finish()).map_err(invalid)?;
    let mut deserializer = serde_json::Deserializer::from_slice(&json);
    UniqueKeys::deserialize(&mut deserializer).map_err(invalid)?;
    deserializer.end().map_err(invalid)?;
    let manifest: Manifest = serde_json::from_slice(&json).map_err(invalid)?;
    let pair = pair::open(&pair_directory)?;
    let boot = File::open(staging.path().join(BOOT)).map_err(invalid)?;
    let expected = verified_manifest(
        &pair,
        &boot,
        PackOptions {
            volumes: manifest
                .volumes
                .iter()
                .map(|volume| volume.mapping.clone())
                .collect(),
            source: manifest.source.clone(),
        },
    )?;
    if manifest != expected {
        return Err(invalid(
            "package manifest differs from verified native artifacts",
        ));
    }
    Ok(VerifiedPackage { staging, manifest })
}

fn copy_exact(input: &mut impl Read, output: &mut impl Write, mut length: u64) -> io::Result<()> {
    let mut buffer = [0; 64 << 10];
    while length != 0 {
        let count = length.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        length -= count as u64;
    }
    Ok(())
}

fn require_eof(input: &mut impl Read) -> io::Result<()> {
    loop {
        match input.read(&mut [0]) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(io::Error::other("trailing package data")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            require_eof(&mut self.inner)?;
            return Ok(0);
        }
        let count = self.remaining.min(buffer.len() as u64) as usize;
        let count = self.inner.read(&mut buffer[..count])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

// Scan before model deserialization: Value and flattened mappings otherwise
// silently replace duplicate keys, including keys equal after JSON unescaping.
struct UniqueKeys;
impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}
impl<'de> Visitor<'de> for UniqueKeys {
    type Value = Self;
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self, A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = access.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            access.next_value::<Self>()?;
        }
        Ok(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self, A::Error> {
        while access.next_element::<Self>()?.is_some() {}
        Ok(self)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }
}

#[cfg(test)]
#[path = "tests/unpack.rs"]
mod tests;
