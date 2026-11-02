use super::*;
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::os::unix::fs::FileExt;

struct Fixture {
    pair: tempfile::TempDir,
    boot: tempfile::NamedTempFile,
    metadata: serde_json::Value,
}

impl Fixture {
    fn new(with_volume: bool) -> Self {
        let pair = pair::tests::fixture();
        let bytes = std::fs::read(pair.path().join(MEMORY)).unwrap();
        let length = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&bytes[160..160 + length]).unwrap();
        let mut boot = tempfile::NamedTempFile::new().unwrap();
        boot.write_all(&[29; 4096]).unwrap();
        metadata["disk_dependencies"][1]["external_readonly_boot"]["sha256"] =
            serde_json::json!(Sha256::digest([29; 4096]).to_vec());
        metadata["regions"][0]["len"] = (1 << 20).into();
        metadata["compatibility"] = serde_json::json!({"native_feature": "preserved"});
        let mut options = crate::BoxOptions::default();
        if with_volume {
            options
                .volumes
                .push(crate::runtime::options::VolumeSpec::bind_mount(
                    "/source-machine-unavailable/data",
                    "/data",
                ));
        }
        metadata["application"] = serde_json::json!({
            "options": options,
            "resources": {"cpus":1,"max_cpus":1,"memory_mib":1,"max_memory_mib":1},
            "local_paths": {"version":2,"boot_disk":"/source-machine-unavailable/boot.ext4",
                "shares":{"uservol0":{"path":"/source-machine-unavailable/data"}}}
        });
        if with_volume {
            metadata["fs"] = serde_json::json!({
                "devices":[{"tag":"uservol0","vm_owned":false,"read_only":false}],
                "sessions":[{"objects":[],"state":{},"init_len":0,"dax_offsets":[]}]
            });
        } else {
            metadata["fs"] = serde_json::json!({"devices":[],"sessions":[]});
        }
        for (tag, vm_owned) in [("BoxLiteShared", false), ("/dev/root", true)] {
            metadata["fs"]["devices"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"tag":tag,"vm_owned":vm_owned,"read_only":false}));
            metadata["fs"]["sessions"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"objects":[],"state":{},"init_len":0,"dax_offsets":[]}));
        }
        let fixture = Self {
            pair,
            boot,
            metadata,
        };
        fixture.seal();
        fixture
    }

    fn seal(&self) {
        pair::tests::seal_payload(self.pair.path(), &self.metadata, &vec![0; 1 << 20]);
    }

    fn options(&self) -> PackOptions {
        PackOptions {
            volumes: if self.metadata["application"]["options"]["volumes"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                vec![]
            } else {
                vec![VolumeMapping {
                    share_tag: "uservol0".into(),
                    volume_id: "original-volume".into(),
                    subpath: Some("tenant/data".into()),
                }]
            },
            source: Some(Source {
                box_id: Some("source-box".into()),
                region: Some("region-a".into()),
            }),
        }
    }

    fn pack(&self, options: PackOptions, limits: Limits) -> BoxliteResult<File> {
        pack(self.pair.path(), self.boot.path(), options, limits)
    }
}

fn limits() -> Limits {
    Limits {
        compressed_bytes: 16 << 20,
        expanded_bytes: 16 << 20,
        memory_bytes: 4 << 20,
        disk_bytes: 4 << 20,
        boot_bytes: 4096,
    }
}

fn decode(file: File) -> Vec<u8> {
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .unwrap()
        .single_frame();
    decoder.window_log_max(26).unwrap();
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).unwrap();
    assert_eq!(decoder.finish().read(&mut [0]).unwrap(), 0);
    bytes
}

#[test]
fn checkpoint_package_encodes_verified_bytes_manifest_and_original_volume() {
    for with_volume in [false, true] {
        let fixture = Fixture::new(with_volume);
        let mut file = fixture.pack(fixture.options(), limits()).unwrap();
        assert_eq!(file.stream_position().unwrap(), 0);
        let decoded = decode(file);
        let mut archive = tar::Archive::new(Cursor::new(decoded));
        let entries: Vec<_> = archive
            .entries()
            .unwrap()
            .raw(true)
            .map(|entry| {
                let mut entry = entry.unwrap();
                assert_eq!(entry.header().entry_type(), tar::EntryType::Regular);
                assert!(entry.header().as_ustar().is_some());
                let name = entry.path_bytes().to_vec();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (name, bytes)
            })
            .collect();
        assert_eq!(
            entries
                .iter()
                .map(|(name, _)| name.as_slice())
                .collect::<Vec<_>>(),
            [
                MANIFEST.as_bytes(),
                MEMORY.as_bytes(),
                DISK.as_bytes(),
                BOOT.as_bytes()
            ]
        );
        let manifest: Manifest = serde_json::from_slice(&entries[0].1).unwrap();
        assert_eq!(manifest.format, "boxlite-memory-checkpoint");
        assert_eq!(manifest.version, 1);
        assert_eq!(
            manifest.compatibility.native,
            fixture.metadata["compatibility"]
        );
        assert_eq!(manifest.compatibility.native_version, 9);
        assert_eq!(manifest.compatibility.application_schema, 1);
        assert_eq!(manifest.compatibility.path_metadata_version, 2);
        assert_eq!(manifest.resources.allocation.memory_mib, 1);
        assert_eq!(manifest.resources.memory_bytes, 1 << 20);
        assert_eq!(
            manifest.source.unwrap().box_id.as_deref(),
            Some("source-box")
        );
        assert_eq!(manifest.volumes.len(), usize::from(with_volume));
        if with_volume {
            let reference = &manifest.volumes[0];
            assert_eq!(reference.mapping.volume_id, "original-volume");
            assert_eq!(reference.mapping.subpath.as_deref(), Some("tenant/data"));
            assert_eq!(reference.mount_path, "/data");
            assert!(!reference.read_only);
        }
        for ((name, bytes), source) in entries[1..].iter().zip([
            fixture.pair.path().join(MEMORY),
            fixture.pair.path().join(DISK),
            fixture.boot.path().to_owned(),
        ]) {
            assert_eq!(*bytes, std::fs::read(source).unwrap());
            let artifact = &manifest.files[std::str::from_utf8(name).unwrap()];
            assert_eq!(artifact.length, bytes.len() as u64);
            assert_eq!(artifact.sha256, hex::encode(Sha256::digest(bytes)));
        }
    }
}

#[test]
fn checkpoint_package_rejects_wrong_boot_corrupt_pair_and_unsupported_metadata() {
    for case in 0..8 {
        let mut fixture = Fixture::new(false);
        match case {
            0 => fixture.boot.as_file().write_all_at(b"!", 0).unwrap(),
            1 => fixture.boot.as_file().set_len(4095).unwrap(),
            2 | 3 => {
                let path = fixture
                    .pair
                    .path()
                    .join(if case == 2 { MEMORY } else { DISK });
                OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .write_all_at(b"!", 104)
                    .unwrap();
            }
            4 => fixture.metadata["application"]["local_paths"]["version"] = 1.into(),
            5 => fixture.metadata["application"]["resources"]["memory_mib"] = 2.into(),
            6 => {
                std::fs::write(fixture.pair.path().join("extra"), b"extra").unwrap();
            }
            _ => {
                std::fs::remove_file(fixture.pair.path().join(DISK)).unwrap();
            }
        }
        if matches!(case, 4 | 5) {
            fixture.seal();
        }
        assert!(
            fixture.pack(fixture.options(), limits()).is_err(),
            "case {case}"
        );
    }
    let fixture = Fixture::new(false);
    let link = tempfile::tempdir().unwrap();
    let boot = link.path().join(BOOT);
    std::os::unix::fs::symlink(fixture.boot.path(), &boot).unwrap();
    assert!(pack(fixture.pair.path(), &boot, fixture.options(), limits()).is_err());
    assert!(
        pack(
            fixture.pair.path(),
            link.path(),
            fixture.options(),
            limits()
        )
        .is_err()
    );
}

#[test]
fn checkpoint_package_rejects_volume_coverage_and_attribute_conflicts() {
    for case in 0..11 {
        let mut fixture = Fixture::new(true);
        let mut options = fixture.options();
        match case {
            0 => options.volumes.clear(),
            1 => options.volumes.push(options.volumes[0].clone()),
            2 => options.volumes[0].share_tag = "uservol1".into(),
            3 => options.volumes[0].volume_id = " ".into(),
            4 => fixture.metadata["fs"]["devices"][0]["read_only"] = true.into(),
            5 => fixture.metadata["fs"]["devices"][0]["tag"] = "unknown".into(),
            6 => {
                let device = fixture.metadata["fs"]["devices"][0].clone();
                let session = fixture.metadata["fs"]["sessions"][0].clone();
                fixture.metadata["fs"]["devices"]
                    .as_array_mut()
                    .unwrap()
                    .push(device);
                fixture.metadata["fs"]["sessions"]
                    .as_array_mut()
                    .unwrap()
                    .push(session);
            }
            7 => fixture.metadata["fs"]["devices"][0]["vm_owned"] = true.into(),
            8 => {
                fixture.metadata["application"]["options"]["volumes"][0] = serde_json::to_value(
                    crate::runtime::options::VolumeSpec::cache_volume("cache", "/data"),
                )
                .unwrap()
            }
            9 => options.volumes[0].share_tag = "uservol00".into(),
            _ => {
                fixture.metadata["application"]["options"]["mounts"] =
                    serde_json::to_value([crate::runtime::options::MountSpec::bind_mount(
                        "/unavailable",
                        "/typed",
                    )])
                    .unwrap()
            }
        }
        fixture.seal();
        assert!(fixture.pack(options, limits()).is_err(), "case {case}");
    }
    for subpath in [
        "",
        "/absolute",
        ".",
        "..",
        "a/../b",
        "a//b",
        "a/",
        "a/./b",
        "a\\b",
        "a\0b",
    ] {
        let fixture = Fixture::new(true);
        let mut options = fixture.options();
        options.volumes[0].subpath = Some(subpath.into());
        assert!(fixture.pack(options, limits()).is_err(), "{subpath:?}");
    }
    let mut fixture = Fixture::new(true);
    fixture.metadata["application"]["options"]["volumes"][0]["read_only"] = true.into();
    fixture.metadata["fs"]["devices"][0]["read_only"] = true.into();
    fixture.seal();
    fixture.pack(fixture.options(), limits()).unwrap();
}

#[test]
fn checkpoint_package_enforces_inclusive_limits_and_manifest_cap_without_overflow() {
    let fixture = Fixture::new(false);
    let file = fixture.pack(fixture.options(), limits()).unwrap();
    let compressed = file.metadata().unwrap().len();
    let expanded = decode(file).len() as u64;
    let exact = Limits {
        compressed_bytes: compressed,
        expanded_bytes: expanded,
        memory_bytes: std::fs::metadata(fixture.pair.path().join(MEMORY))
            .unwrap()
            .len(),
        disk_bytes: std::fs::metadata(fixture.pair.path().join(DISK))
            .unwrap()
            .len(),
        boot_bytes: 4096,
    };
    fixture.pack(fixture.options(), exact).unwrap();
    for field in 0..5 {
        let mut smaller = exact;
        match field {
            0 => smaller.compressed_bytes -= 1,
            1 => smaller.expanded_bytes -= 1,
            2 => smaller.memory_bytes -= 1,
            3 => smaller.disk_bytes -= 1,
            _ => smaller.boot_bytes -= 1,
        }
        assert!(
            fixture.pack(fixture.options(), smaller).is_err(),
            "field {field}"
        );
    }
    fixture
        .pack(
            fixture.options(),
            Limits {
                compressed_bytes: u64::MAX,
                expanded_bytes: u64::MAX,
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                boot_bytes: u64::MAX,
            },
        )
        .unwrap();
    let mut options = fixture.options();
    options.source.as_mut().unwrap().region = Some("x".repeat(MANIFEST_LIMIT as usize));
    assert!(
        fixture
            .pack(options, limits())
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
}

struct InterruptedReader(bool);
impl Read for InterruptedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if std::mem::replace(&mut self.0, false) {
            Err(io::ErrorKind::Interrupted.into())
        } else {
            buffer[0] = 1;
            Ok(1)
        }
    }
}

#[derive(Debug)]
struct FailingWriter;
impl Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::StorageFull.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn checkpoint_package_propagates_copy_and_encoder_io_failures() {
    assert!(
        copy_bounded(InterruptedReader(true), Vec::new(), 1)
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
    assert_eq!(
        copy_bounded(Cursor::new(b"data"), FailingWriter, 4)
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
    let unreadable = File::create(tempfile::NamedTempFile::new().unwrap().path()).unwrap();
    assert!(copy_bounded(unreadable, Vec::new(), 4).is_err());
    let fixture = Fixture::new(false);
    let pair = pair::open(fixture.pair.path()).unwrap();
    let boot = File::open(fixture.boot.path()).unwrap();
    assert_eq!(
        encode(
            FailingWriter,
            b"{}",
            [&pair.memory, &pair.disk, &boot],
            limits()
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::StorageFull
    );
    // A read-only output exercises the actual filesystem write failure path.
    let output = File::open(fixture.boot.path()).unwrap();
    assert!(encode(output, b"{}", [&pair.memory, &pair.disk, &boot], limits()).is_err());
}

#[test]
fn checkpoint_package_cleans_owned_staging_on_success_and_failure() {
    let fixture = Fixture::new(false);
    for case in 0..4 {
        let staging = tempfile::tempdir().unwrap();
        let path = staging.path().to_owned();
        let (memory, disk) = pair::open_files(fixture.pair.path()).unwrap();
        let boot = File::open(fixture.boot.path()).unwrap();
        let mut limits = limits();
        match case {
            1 => limits.compressed_bytes = 0,
            2 => limits.memory_bytes = 0,
            3 => {
                std::fs::create_dir(path.join("pair")).unwrap();
            }
            _ => {}
        }
        let result = pack_staged(staging, [memory, disk, boot], fixture.options(), limits);
        assert_eq!(result.is_ok(), case == 0);
        assert!(!path.exists(), "staging remained for case {case}");
        if let Ok(file) = result {
            // Unix anonymous output has no remaining directory entry.
            use std::os::unix::fs::MetadataExt;
            assert_eq!(file.metadata().unwrap().nlink(), 0);
        }
    }
}

#[test]
fn checkpoint_package_stages_pinned_descriptors_despite_source_path_replacement() {
    let fixture = Fixture::new(false);
    let (memory, disk) = pair::open_files(fixture.pair.path()).unwrap();
    let boot = File::open(fixture.boot.path()).unwrap();
    let originals = tempfile::tempdir().unwrap();
    for (name, path) in [
        (MEMORY, fixture.pair.path().join(MEMORY)),
        (DISK, fixture.pair.path().join(DISK)),
        (BOOT, fixture.boot.path().to_owned()),
    ] {
        std::fs::rename(&path, originals.path().join(name)).unwrap();
        std::fs::write(path, b"replaced after open").unwrap();
    }
    let output = pack_staged(
        tempfile::tempdir().unwrap(),
        [memory, disk, boot],
        fixture.options(),
        limits(),
    )
    .unwrap();
    let mut archive = tar::Archive::new(Cursor::new(decode(output)));
    for entry in archive.entries().unwrap().skip(1) {
        let mut entry = entry.unwrap();
        let source = originals.path().join(entry.path().unwrap());
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, std::fs::read(source).unwrap());
    }
}
