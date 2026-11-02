//! Local cloud-package codec. Integrity does not imply restore compatibility.
use crate::litebox::checkpoint::pair::{self, DISK, MEMORY, invalid};
use crate::{BoxResources, BoxliteResult};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MANIFEST: &str = "manifest.json";
const BOOT: &str = "boot.ext4";
const MANIFEST_LIMIT: u64 = 16 << 20;

mod unpack;
pub use unpack::{VerifiedPackage, unpack};

/// Inclusive byte limits, required even for local use. No unlimited default.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub compressed_bytes: u64,
    /// Includes tar headers, member padding and footer.
    pub expanded_bytes: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub boot_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMapping {
    pub share_tag: String,
    pub volume_id: String,
    /// Already normalized relative path, or None for the volume root.
    pub subpath: Option<String>,
}

/// Informational provenance; never establishes ownership or authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub box_id: Option<String>,
    pub region: Option<String>,
}

#[derive(Debug, Default)]
pub struct PackOptions {
    pub volumes: Vec<VolumeMapping>,
    pub source: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub length: u64,
    pub sha256: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    pub native_version: u32,
    pub application_schema: u32,
    pub path_metadata_version: u32,
    pub native: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub allocation: BoxResources,
    pub cpus: u16,
    pub memory_bytes: u64,
    pub boot_memory_mib: u32,
    pub max_memory_mib: u32,
    pub container_disk_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeReference {
    #[serde(flatten)]
    pub mapping: VolumeMapping,
    pub mount_path: String,
    pub read_only: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub files: BTreeMap<String, Artifact>,
    pub compatibility: Compatibility,
    pub resources: Resources,
    pub volumes: Vec<VolumeReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

/// Copy pinned inputs, verify private copies, then return a finished anonymous
/// tar+zstd file positioned at zero. Call from a blocking thread in async code.
pub fn pack(
    pair_directory: &Path,
    boot_disk: &Path,
    options: PackOptions,
    limits: Limits,
) -> BoxliteResult<File> {
    let (memory, disk) = pair::open_files(pair_directory)?;
    let boot = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(boot_disk)
        .map_err(invalid)?;
    let staging = tempfile::Builder::new()
        .prefix(".boxlite-checkpoint-package-")
        .tempdir()
        .map_err(invalid)?;
    pack_staged(staging, [memory, disk, boot], options, limits)
}

fn pack_staged(
    staging: tempfile::TempDir,
    inputs: [File; 3],
    options: PackOptions,
    limits: Limits,
) -> BoxliteResult<File> {
    let [memory, disk, boot] = inputs;
    let pair_directory = staging.path().join("pair");
    std::fs::create_dir(&pair_directory).map_err(invalid)?;
    let mut remaining = limits.expanded_bytes;
    for (file, path, limit) in [
        (memory, pair_directory.join(MEMORY), limits.memory_bytes),
        (disk, pair_directory.join(DISK), limits.disk_bytes),
        (boot, staging.path().join(BOOT), limits.boot_bytes),
    ] {
        let metadata = file.metadata().map_err(invalid)?;
        if !metadata.is_file() {
            return Err(invalid("package input must be a regular file"));
        }
        let limit = limit.min(remaining);
        if metadata.len() > limit {
            return Err(invalid("package input exceeds byte limit"));
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(invalid)?;
        remaining -= copy_bounded(file, &mut output, limit).map_err(invalid)?;
    }
    let pair = pair::open(&pair_directory)?;
    let boot = File::open(staging.path().join(BOOT)).map_err(invalid)?;
    let manifest = verified_manifest(&pair, &boot, options)?;
    let mut json = BoundedWriter {
        inner: Vec::new(),
        remaining: MANIFEST_LIMIT,
    };
    serde_json::to_writer(&mut json, &manifest).map_err(invalid)?;
    let output = tempfile::tempfile().map_err(invalid)?;
    encode(
        output,
        &json.inner,
        [&pair.memory, &pair.disk, &boot],
        limits,
    )
    .map_err(invalid)
    .and_then(|mut output| {
        output.sync_all().map_err(invalid)?;
        output.seek(SeekFrom::Start(0)).map_err(invalid)?;
        Ok(output)
    })
}

fn verified_manifest(
    pair: &pair::VerifiedPair,
    boot: &File,
    options: PackOptions,
) -> BoxliteResult<Manifest> {
    pair.verify_disk()?;
    let dependencies = &pair.dependencies;
    if dependencies.application["local_paths"]["version"] != 2 {
        return Err(invalid("package requires checkpoint path metadata v2"));
    }
    let allocation =
        serde_json::from_value(dependencies.application["resources"].clone()).map_err(invalid)?;
    pair::validate_resources(allocation, dependencies)?;
    let files = [(MEMORY, &pair.memory), (DISK, &pair.disk), (BOOT, boot)]
        .into_iter()
        .map(|(name, file)| {
            let length = file.metadata().map_err(invalid)?.len();
            Ok((
                name.to_owned(),
                Artifact {
                    length,
                    sha256: hex::encode(pair::digest(file, 0, length)?),
                },
            ))
        })
        .collect::<BoxliteResult<BTreeMap<_, _>>>()?;
    if files[BOOT].length != dependencies.boot_disk_bytes
        || files[BOOT].sha256 != dependencies.boot_disk_sha256
    {
        return Err(invalid("package boot length/checksum mismatch"));
    }
    Ok(Manifest {
        format: "boxlite-memory-checkpoint".into(),
        version: 1,
        files,
        compatibility: Compatibility {
            native_version: dependencies.version,
            application_schema: 1,
            path_metadata_version: 2,
            native: dependencies.compatibility.clone(),
        },
        resources: Resources {
            allocation,
            cpus: dependencies.cpus,
            memory_bytes: dependencies.memory_bytes,
            boot_memory_mib: dependencies.boot_memory_mib,
            max_memory_mib: dependencies.max_memory_mib,
            container_disk_bytes: dependencies.container_disk_bytes,
        },
        volumes: volumes(dependencies, options.volumes)?,
        source: options.source,
    })
}

fn volumes(
    dependencies: &pair::CheckpointDependencies,
    mappings: Vec<VolumeMapping>,
) -> BoxliteResult<Vec<VolumeReference>> {
    let options: crate::BoxOptions =
        serde_json::from_value(dependencies.application["options"].clone()).map_err(invalid)?;
    if !options.mounts.is_empty() {
        return Err(invalid("package does not support typed mounts"));
    }
    let mut shares = BTreeMap::new();
    for share in &dependencies.shares {
        if (share.vm_owned && share.tag != "/dev/root")
            || (share.tag == crate::runtime::constants::mount_tags::SHARED
                && (share.vm_owned || share.read_only))
        {
            return Err(invalid("unsupported internal share attributes"));
        }
        if shares.insert(share.tag.as_str(), share).is_some() {
            return Err(invalid("duplicate native share tag"));
        }
    }
    let mut tags = BTreeSet::new();
    let mut references = Vec::new();
    for mapping in mappings {
        if !tags.insert(mapping.share_tag.clone()) || mapping.volume_id.trim().is_empty() {
            return Err(invalid("duplicate volume mapping or empty volume ID"));
        }
        if let Some(path) = &mapping.subpath
            && (path.contains(['\\', '\0'])
                || path.split('/').any(|part| matches!(part, "" | "." | "..")))
        {
            return Err(invalid("volume subpath must be normalized and relative"));
        }
        let index = mapping
            .share_tag
            .strip_prefix("uservol")
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| mapping.share_tag == format!("uservol{index}"))
            .ok_or_else(|| invalid("unknown volume mapping tag"))?;
        let volume = options
            .volumes
            .get(index)
            .ok_or_else(|| invalid("extra volume mapping"))?;
        volume.validate()?;
        let share = shares
            .get(mapping.share_tag.as_str())
            .ok_or_else(|| invalid("volume share missing"))?;
        if volume.cache_volume.is_some() || share.vm_owned || share.read_only != volume.read_only {
            return Err(invalid("volume origin or native mount attributes conflict"));
        }
        references.push(VolumeReference {
            mapping,
            mount_path: volume.guest_path.clone(),
            read_only: volume.read_only,
        });
    }
    if references.len() != options.volumes.len()
        || shares.values().any(|share| {
            !tags.contains(&share.tag)
                && !(share.vm_owned || share.tag == crate::runtime::constants::mount_tags::SHARED)
        })
    {
        return Err(invalid(
            "volume mappings must cover exactly the external shares",
        ));
    }
    Ok(references)
}

fn copy_bounded(mut input: impl Read, output: impl Write, limit: u64) -> io::Result<u64> {
    let copied = io::copy(
        &mut input.by_ref().take(limit),
        &mut BoundedWriter {
            inner: output,
            remaining: limit,
        },
    )?;
    if input.read(&mut [0])? != 0 {
        return Err(io::Error::other(
            "checkpoint package input exceeds byte limit",
        ));
    }
    Ok(copied)
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() as u64 > self.remaining {
            return Err(io::Error::other("checkpoint package exceeds byte limit"));
        }
        let count = self.inner.write(buffer)?;
        self.remaining -= count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn encode<W: Write>(output: W, json: &[u8], files: [&File; 3], limits: Limits) -> io::Result<W> {
    let output = BoundedWriter {
        inner: output,
        remaining: limits.compressed_bytes,
    };
    let mut encoder = zstd::stream::write::Encoder::new(output, 1)?;
    encoder.include_checksum(true)?;
    encoder.window_log(26)?; // 2^26 = 64 MiB maximum back-reference window.
    let mut archive = tar::Builder::new(BoundedWriter {
        inner: encoder,
        remaining: limits.expanded_bytes,
    });
    append(&mut archive, MANIFEST, json.len() as u64, json)?;
    for (name, mut file) in [MEMORY, DISK, BOOT].into_iter().zip(files) {
        file.rewind()?;
        append(&mut archive, name, file.metadata()?.len(), file)?;
    }
    Ok(archive.into_inner()?.inner.finish()?.inner)
}

fn append(
    archive: &mut tar::Builder<impl Write>,
    name: &str,
    size: u64,
    input: impl Read,
) -> io::Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_path(name)?;
    header.set_size(size);
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    archive.append(&header, input)
}

#[cfg(test)]
#[path = "tests/pack.rs"]
mod tests;
