use super::*;
use crate::checkpoint_package::tests::{Fixture, limits};
use sha2::{Digest, Sha256};
use std::io::Cursor;

fn bytes(mut input: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes).unwrap();
    bytes
}

fn archive(raw: &[u8]) -> File {
    compressed_file(&zstd::stream::encode_all(raw, 1).unwrap())
}

fn compressed_file(bytes: &[u8]) -> File {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    // Deliberately leave at EOF: unpack must rewind its input.
    file
}

fn raw_package(fixture: &Fixture) -> Vec<u8> {
    bytes(
        zstd::stream::read::Decoder::new(fixture.pack(fixture.options(), limits()).unwrap())
            .unwrap(),
    )
}

fn members(raw: &[u8]) -> Vec<(tar::Header, Vec<u8>)> {
    tar::Archive::new(Cursor::new(raw))
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let header = entry.header().clone();
            (header, bytes(entry))
        })
        .collect()
}

fn rebuild(members: &[(tar::Header, Vec<u8>)]) -> Vec<u8> {
    let mut raw = Vec::new();
    for (header, body) in members {
        let mut header = header.clone();
        header.set_size(body.len() as u64);
        header.set_cksum();
        raw.extend(header.as_bytes());
        raw.extend(body);
        raw.resize(raw.len().next_multiple_of(512), 0);
    }
    raw.resize(raw.len() + 1024, 0);
    raw
}

fn set_json(members: &mut [(tar::Header, Vec<u8>)], edit: impl FnOnce(&mut serde_json::Value)) {
    let mut json = serde_json::from_slice(&members[0].1).unwrap();
    edit(&mut json);
    members[0].1 = serde_json::to_vec(&json).unwrap();
}

fn reject(raw: &[u8]) -> String {
    unpack(archive(raw), limits()).unwrap_err().to_string()
}

#[test]
fn checkpoint_package_unpack_roundtrip_preserves_bytes_volumes_and_owner_lifetime() {
    for with_volume in [false, true] {
        let mut fixture = Fixture::new(with_volume);
        if with_volume {
            fixture.metadata["application"]["options"]["volumes"][0]["read_only"] = true.into();
            fixture.metadata["fs"]["devices"][0]["read_only"] = true.into();
            fixture.seal();
        }
        let inputs = [
            fixture.pair.path().join(MEMORY),
            fixture.pair.path().join(DISK),
            fixture.boot.path().into(),
        ];
        let expected = inputs
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>();
        let file = fixture.pack(fixture.options(), limits()).unwrap();
        drop(fixture);
        let verified = unpack(file, limits()).unwrap();
        let pair_path = verified.pair_directory();
        let boot_path = verified.boot_disk();
        for (path, expected) in [
            pair_path.join(MEMORY),
            pair_path.join(DISK),
            boot_path.clone(),
        ]
        .iter()
        .zip(expected)
        {
            assert_eq!(std::fs::read(path).unwrap(), expected);
        }
        assert_eq!(verified.manifest().volumes.len(), usize::from(with_volume));
        if with_volume {
            let volume = &verified.manifest().volumes[0];
            assert_eq!(volume.mapping.volume_id, "original-volume");
            assert_eq!(volume.mapping.subpath.as_deref(), Some("tenant/data"));
            assert_eq!(volume.mount_path, "/data");
            assert!(volume.read_only);
        }
        assert!(!Path::new("/source-machine-unavailable/boot.ext4").exists());
        assert!(!Path::new("/source-machine-unavailable/data").exists());
        drop(verified);
        assert!(!pair_path.exists());
        assert!(!boot_path.exists());
    }
}

#[test]
fn checkpoint_package_unpack_accepts_member_permutations_and_base256() {
    let fixture = Fixture::new(false);
    let mut entries = members(&raw_package(&fixture));
    entries.rotate_left(2);
    let raw = rebuild(&entries);
    unpack(archive(&raw), limits()).unwrap();
    let mut entries = members(&raw_package(&fixture));
    let mut raw = rebuild(&entries);
    // Force the nonnegative base-256 representation on a real production body.
    let mut header = entries.remove(0).0;
    let length = header.size().unwrap();
    header.as_mut_bytes()[124..136].fill(0);
    header.as_mut_bytes()[124] = 0x80;
    header.as_mut_bytes()[128..136].copy_from_slice(&length.to_be_bytes());
    header.set_cksum();
    raw[..512].copy_from_slice(header.as_bytes());
    unpack(archive(&raw), limits()).unwrap();
}

#[test]
fn checkpoint_package_unpack_rejects_manifest_disagreement_and_unsupported_contracts() {
    let fixture = Fixture::new(true);
    let original = members(&raw_package(&fixture));
    let cases = [
        ("/format", serde_json::json!("other")),
        ("/version", serde_json::json!(2)),
        ("/compatibility/native_version", serde_json::json!(8)),
        ("/compatibility/application_schema", serde_json::json!(2)),
        ("/compatibility/path_metadata_version", serde_json::json!(1)),
        (
            "/compatibility/native/native_feature",
            serde_json::json!("changed"),
        ),
        ("/resources/allocation/cpus", serde_json::json!(2)),
        ("/resources/allocation/max_cpus", serde_json::json!(2)),
        ("/resources/allocation/memory_mib", serde_json::json!(2)),
        ("/resources/allocation/max_memory_mib", serde_json::json!(2)),
        ("/resources/cpus", serde_json::json!(2)),
        ("/resources/memory_bytes", serde_json::json!(2)),
        ("/resources/boot_memory_mib", serde_json::json!(2)),
        ("/resources/max_memory_mib", serde_json::json!(2)),
        ("/resources/container_disk_bytes", serde_json::json!(2)),
        ("/volumes/0/mount_path", serde_json::json!("/other")),
        ("/volumes/0/read_only", serde_json::json!(true)),
        ("/volumes/0/share_tag", serde_json::json!("unknown")),
        ("/volumes/0/volume_id", serde_json::json!("")),
        ("/volumes/0/subpath", serde_json::json!("../escape")),
        ("/files/boot.ext4/length", serde_json::json!(4095)),
        (
            "/files/snapshot.mem/sha256",
            serde_json::json!("0".repeat(64)),
        ),
    ];
    for (pointer, value) in cases {
        let mut entries = original.clone();
        set_json(&mut entries, |json| {
            *json.pointer_mut(pointer).unwrap() = value
        });
        assert!(!reject(&rebuild(&entries)).is_empty(), "{pointer}");
    }
    for case in 0..6 {
        let mut entries = original.clone();
        set_json(&mut entries, |json| match case {
            0 => {
                json["unknown"] = true.into();
            }
            1 => {
                json["files"].as_object_mut().unwrap().remove(BOOT);
            }
            2 => {
                json["files"]["extra"] = json["files"][BOOT].clone();
            }
            3 => {
                json["volumes"] = serde_json::json!([]);
            }
            4 => {
                let volume = json["volumes"][0].clone();
                json["volumes"].as_array_mut().unwrap().push(volume);
            }
            _ => {
                json.as_object_mut().unwrap().remove("resources");
            }
        });
        reject(&rebuild(&entries));
    }
    let mut entries = original;
    set_json(&mut entries, |json| {
        json["source"] = serde_json::json!({"box_id":"untrusted","region":"elsewhere"})
    });
    unpack(archive(&rebuild(&entries)), limits()).unwrap();
}

#[test]
fn checkpoint_package_unpack_detects_payload_corruption_even_with_updated_outer_hashes() {
    let fixture = Fixture::new(false);
    let original = members(&raw_package(&fixture));
    for artifact in 1..4 {
        for rehash in [false, true] {
            let mut entries = original.clone();
            entries[artifact].1[104] ^= 1;
            if rehash {
                let name = [MEMORY, DISK, BOOT][artifact - 1];
                let digest = hex::encode(Sha256::digest(&entries[artifact].1));
                set_json(&mut entries, |json| {
                    json["files"][name]["sha256"] = digest.into()
                });
            }
            let error = reject(&rebuild(&entries));
            if rehash {
                assert!(error.contains("checksum"), "{error}");
            }
        }
    }
    // Reseal native envelope and outer digest: invalid saved allocation remains invalid.
    let mut fixture = fixture;
    fixture.metadata["application"]["resources"]["cpus"] = 0.into();
    fixture.seal();
    let mut entries = original;
    entries[1].1 = std::fs::read(fixture.pair.path().join(MEMORY)).unwrap();
    let digest = hex::encode(Sha256::digest(&entries[1].1));
    let length = entries[1].1.len();
    set_json(&mut entries, |json| {
        json["files"][MEMORY] = serde_json::json!({"length":length,"sha256":digest})
    });
    assert!(reject(&rebuild(&entries)).contains("allocation"));
}

#[test]
fn checkpoint_package_unpack_rejects_missing_duplicate_extra_and_unsafe_headers() {
    let fixture = Fixture::new(false);
    let original = members(&raw_package(&fixture));
    for case in 0..3 {
        let mut entries = original.clone();
        match case {
            0 => {
                entries.remove(1);
            }
            1 => {
                entries[2] = entries[1].clone();
            }
            _ => entries.push(entries[1].clone()),
        }
        reject(&rebuild(&entries));
    }
    for path in [
        "../snapshot.mem",
        "./snapshot.mem",
        "/snapshot.mem",
        "pair/snapshot.mem",
        "snapshot.mem/",
        "snapshot.mem\\x",
    ] {
        let mut entries = original.clone();
        entries[1].0.as_mut_bytes()[..100].fill(0);
        entries[1].0.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        reject(&rebuild(&entries));
    }
    for kind in *b"1234567gxLKS" {
        let mut entries = original.clone();
        entries[1].0.set_entry_type(tar::EntryType::new(kind));
        reject(&rebuild(&entries));
    }
    for (offset, byte) in [
        (157, b'x'),
        (345, b'x'),
        (257, b'x'),
        (263, b'1'),
        (90, b'x'),
    ] {
        let mut entries = original.clone();
        entries[1].0.as_mut_bytes()[offset] = byte;
        reject(&rebuild(&entries));
    }
    let mut raw = rebuild(&original);
    raw[10] ^= 1;
    assert!(reject(&raw).contains("header"));
}

#[test]
fn checkpoint_package_unpack_rejects_duplicate_json_escaped_keys_depth_and_trailing_json() {
    let fixture = Fixture::new(false);
    let original = members(&raw_package(&fixture));
    for extra in [
        r#""source":{"region":"a","region":"b"}"#,
        r#""source":{"region":"a","reg\u0069on":"b"}"#,
        r#""source":{"ignored":[{"a":0,"a":1}]}"#,
        r#""format":"boxlite-memory-checkpoint""#,
    ] {
        let mut entries = original.clone();
        let json = String::from_utf8(entries[0].1.clone()).unwrap();
        // Remove original source so nested duplicates are the only failure signal.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("source");
        let json = serde_json::to_string(&value).unwrap();
        entries[0].1 = format!("{{{extra},{}", &json[1..]).into_bytes();
        assert!(reject(&rebuild(&entries)).contains("duplicate JSON key"));
    }
    let mut entries = original.clone();
    let json = String::from_utf8(entries[0].1.clone()).unwrap();
    entries[0].1 = format!(
        "{{\"deep\":{}0{},{}",
        "[".repeat(128),
        "]".repeat(128),
        &json[1..]
    )
    .into_bytes();
    assert!(reject(&rebuild(&entries)).contains("recursion limit"));
    for suffix in [b" {}".as_slice(), b"!", b"\0"] {
        let mut entries = original.clone();
        entries[0].1.extend(suffix);
        reject(&rebuild(&entries));
    }
    let mut entries = original;
    entries[0].1[0] = b'!';
    reject(&rebuild(&entries));
}

#[test]
fn checkpoint_package_unpack_rejects_tar_and_frame_truncation_padding_and_trailers() {
    let fixture = Fixture::new(false);
    let raw = raw_package(&fixture);
    let entries = members(&raw);
    let first_length = entries[0].1.len();
    let first_end = (512 + first_length).next_multiple_of(512);
    for cut in [
        0,
        511,
        512,
        512 + first_length - 1,
        first_end - 1,
        raw.len() - 1024,
        raw.len() - 512,
        raw.len() - 1,
    ] {
        reject(&raw[..cut]);
    }
    for index in [512 + first_length, raw.len() - 1024, raw.len() - 1] {
        let mut modified = raw.clone();
        modified[index] = 1;
        reject(&modified);
    }
    for trailer in [vec![0; 512], b"hidden".to_vec(), vec![0; 1024]] {
        let mut modified = raw.clone();
        modified.extend(trailer);
        assert!(reject(&modified).contains("trailing"));
    }
    let mut compressed = archive(&raw);
    compressed.rewind().unwrap();
    let compressed = bytes(compressed);
    for cut in [0, 3, 4, compressed.len() / 2, compressed.len() - 1] {
        assert!(unpack(compressed_file(&compressed[..cut]), limits()).is_err());
    }
    let mut skippable = vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
    skippable.extend(&compressed);
    assert!(unpack(compressed_file(&skippable), limits()).is_err());
    for trailer in [
        compressed.clone(),
        vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
        vec![1],
        vec![0; 200_000],
    ] {
        let mut modified = compressed.clone();
        modified.extend(trailer);
        assert!(unpack(compressed_file(&modified), limits()).is_err());
    }
    // A legal ordinary frame advertising a 128 MiB window, with tiny body.
    let mut wide_window = compressed;
    assert_eq!(wide_window[4] & 0x20, 0); // streaming encoder, no single-segment flag
    wide_window[5] = 17 << 3; // 2^(10+17) bytes
    let error = unpack(compressed_file(&wide_window), limits())
        .unwrap_err()
        .to_string();
    assert!(error.contains("too much memory"), "{error}");
}

#[test]
fn checkpoint_package_unpack_enforces_exact_limits_and_numeric_boundaries() {
    let fixture = Fixture::new(false);
    let raw = raw_package(&fixture);
    let compressed = zstd::stream::encode_all(raw.as_slice(), 1).unwrap();
    let entries = members(&raw);
    let exact = Limits {
        compressed_bytes: compressed.len() as u64,
        expanded_bytes: raw.len() as u64,
        memory_bytes: entries[1].1.len() as u64,
        disk_bytes: entries[2].1.len() as u64,
        boot_bytes: entries[3].1.len() as u64,
    };
    unpack(compressed_file(&compressed), exact).unwrap();
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
            unpack(compressed_file(&compressed), smaller).is_err(),
            "{field}"
        );
    }
    let unlimited = Limits {
        compressed_bytes: u64::MAX,
        expanded_bytes: u64::MAX,
        memory_bytes: u64::MAX,
        disk_bytes: u64::MAX,
        boot_bytes: u64::MAX,
    };
    unpack(compressed_file(&compressed), unlimited).unwrap();
    for size in [
        [0xff; 12],
        [0x80, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1],
        [
            0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
    ] {
        let mut modified = raw.clone();
        let mut header = entries[1].0.clone();
        header.as_mut_bytes()[124..136].copy_from_slice(&size);
        header.set_cksum();
        let offset = (512 + entries[0].1.len()).next_multiple_of(512);
        modified[offset..offset + 512].copy_from_slice(header.as_bytes());
        let error = unpack(archive(&modified), unlimited)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("overflows") || error.contains("byte limit"),
            "{error}"
        );
    }
    for length in [MANIFEST_LIMIT, MANIFEST_LIMIT + 1] {
        let mut entries = entries.clone();
        // Whitespace is valid JSON and lets the real pack manifest reach the cap exactly.
        entries[0].1.resize(length as usize, b' ');
        let modified = rebuild(&entries);
        let result = unpack(archive(&modified), unlimited);
        assert_eq!(result.is_ok(), length == MANIFEST_LIMIT);
    }
}

struct ReadFailure;
impl Read for ReadFailure {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::Other.into())
    }
}

#[test]
fn checkpoint_package_unpack_cleans_staging_on_every_failure_and_propagates_io_errors() {
    let fixture = Fixture::new(false);
    let raw = raw_package(&fixture);
    let mut corrupt = members(&raw);
    corrupt[3].1[0] ^= 1;
    let mut bad_json = members(&raw);
    bad_json[0].1[0] = b'!';
    let mut bad_padding = raw.clone();
    bad_padding[512 + members(&raw)[0].1.len()] = 1;
    for input in [
        vec![],
        raw[..511].to_vec(),
        raw[..raw.len() - 1].to_vec(),
        bad_padding,
        rebuild(&bad_json),
        rebuild(&corrupt),
        raw.clone(),
    ] {
        let staging = tempfile::tempdir().unwrap();
        let path = staging.path().to_owned();
        let mut compressed = archive(&input);
        compressed.rewind().unwrap();
        let result = unpack_staged(compressed, limits(), staging);
        if input == raw {
            let verified = result.unwrap();
            assert!(path.exists());
            drop(verified);
        } else {
            assert!(result.is_err());
        }
        assert!(!path.exists());
    }
    for case in 0..4 {
        let staging = tempfile::tempdir().unwrap();
        let path = staging.path().to_owned();
        match case {
            0 => std::fs::create_dir(path.join("pair")).unwrap(),
            1 => std::fs::create_dir(path.join(BOOT)).unwrap(),
            2 => {
                std::fs::remove_dir(&path).unwrap();
                std::fs::write(&path, b"sentinel").unwrap();
            }
            _ => {}
        }
        let mut input = archive(&raw);
        input.rewind().unwrap();
        let result = if case == 3 {
            unpack_staged(ReadFailure, limits(), staging)
        } else {
            unpack_staged(input, limits(), staging)
        };
        assert!(result.is_err());
        if case == 2 {
            assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
            std::fs::remove_file(path).unwrap();
        } else {
            assert!(!path.exists());
        }
    }
    let unreadable = File::create(fixture.boot.path()).unwrap();
    assert!(unpack(unreadable, limits()).is_err());
    let mut output = File::open(fixture.boot.path()).unwrap();
    assert!(copy_exact(&mut Cursor::new(&raw), &mut output, 512).is_err());
    assert!(copy_exact(&mut ReadFailure, &mut Vec::new(), 512).is_err());
}
