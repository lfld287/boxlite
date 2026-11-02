use super::*;
use crate::runtime::options::BoxliteOptions;
use crate::runtime::rt_impl::RuntimeImpl;
use std::io::Write;
use std::os::fd::AsRawFd;
use tokio_util::sync::CancellationToken;

struct Fixture {
    pair: tempfile::TempDir,
    boot: tempfile::NamedTempFile,
    metadata: serde_json::Value,
}

impl Fixture {
    fn new() -> Self {
        let pair = pair::tests::fixture();
        let bytes = std::fs::read(pair.path().join(pair::MEMORY)).unwrap();
        let length = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&bytes[160..160 + length]).unwrap();
        let mut boot = tempfile::NamedTempFile::new().unwrap();
        boot.write_all(&[29; 4096]).unwrap();
        metadata["disk_dependencies"][1]["external_readonly_boot"]["sha256"] =
            serde_json::json!(pair::digest(boot.as_file(), 0, 4096).unwrap());
        metadata["regions"][0]["len"] = (1 << 20).into();
        metadata["application"] = serde_json::json!(Application {
            source_box_id: "source".into(),
            guest: crate::litebox::checkpoint::GuestIdentity {
                boot_id: "boot-1".into(),
                container_id: ContainerID::new().to_string(),
                init_pid: 42,
            },
            options: crate::BoxOptions::default(),
            resources: crate::BoxResources {
                cpus: 1,
                max_cpus: 1,
                memory_mib: 1,
                max_memory_mib: 1
            },
            container_image_config: Default::default(),
            outbound_policy: None,
            local_paths: Some(LocalPaths {
                version: 2,
                boot_disk: boot.path().into(),
                shares: Default::default()
            }),
        });
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

    fn paths(&self) -> LocalPaths {
        serde_json::from_value(self.metadata["application"]["local_paths"].clone()).unwrap()
    }

    fn open(&self) -> pair::VerifiedPair {
        let pair = pair::open(self.pair.path()).unwrap();
        pair.verify_disk().unwrap();
        pair
    }
}

fn runtime(home: &Path) -> SharedRuntimeImpl {
    RuntimeImpl::new_for_test(BoxliteOptions {
        home_dir: home.into(),
        image_registries: vec![],
        ..Default::default()
    })
    .unwrap()
}

fn attempt(runtime: &SharedRuntimeImpl) -> Attempt {
    let layout = runtime
        .layout
        .box_layout(BoxIDMint::mint().as_str(), false)
        .unwrap();
    std::fs::create_dir(layout.root()).unwrap();
    layout.prepare().unwrap();
    Attempt {
        runtime: runtime.clone(),
        layout,
        handler: None,
        lock: None,
        armed: true,
    }
}

#[test]
fn checkpoint_packaged_boot_survives_package_drop_and_persisted_storage_reload() {
    use crate::checkpoint_package::{self, Limits, PackOptions};
    let home = tempfile::tempdir().unwrap();
    let runtime = runtime(home.path());
    let fixture = Fixture::new();
    let limits = Limits {
        compressed_bytes: 8 << 20,
        expanded_bytes: 8 << 20,
        memory_bytes: 4 << 20,
        disk_bytes: 4 << 20,
        boot_bytes: 4096,
    };
    let archive = checkpoint_package::pack(
        fixture.pair.path(),
        fixture.boot.path(),
        PackOptions::default(),
        limits,
    )
    .unwrap();
    let saved_paths = fixture.paths();
    drop(fixture);
    assert!(!saved_paths.boot_disk.exists());
    let package = checkpoint_package::unpack(archive, limits).unwrap();
    let source_boot = package.boot_disk();
    let pair = pair::open(&package.pair_directory()).unwrap();
    pair.verify_disk().unwrap();
    let (mut attempt, _, boot) = prepare_disks(
        attempt(&runtime),
        pair,
        &saved_paths,
        Some(&source_boot),
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(boot.path.starts_with(attempt.layout.root()));
    assert_ne!(boot.path, source_boot);
    assert!(boot.dependency.base_id.is_none());
    // SAFETY: boot.file is a live descriptor; F_GETFL does not modify it.
    let flags = unsafe { libc::fcntl(boot.file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    let boot_path = boot.path.clone();
    drop(package);
    assert!(!source_boot.exists());
    assert_eq!(std::fs::read(&boot_path).unwrap(), [29; 4096]);

    let id = BoxID::parse(attempt.layout.root().file_name().unwrap().to_str().unwrap()).unwrap();
    let config = BoxConfig {
        id: id.clone(),
        name: None,
        created_at: chrono::Utc::now(),
        container: ContainerRuntimeConfig {
            id: ContainerID::new(),
        },
        options: Default::default(),
        engine_kind: VmmKind::Libkrun,
        box_home: attempt.layout.root().into(),
    };
    let mut state = BoxState::new();
    state.set_status(BoxStatus::Stopped);
    state.restored_storage = Some(RestoredStorage {
        disks: vec![
            RestoredDisk {
                path: attempt.layout.disks_dir().join("disk.qcow2"),
                format: crate::vmm::DiskFormat::Qcow2,
                read_only: false,
            },
            RestoredDisk {
                path: boot.path.clone(),
                format: crate::vmm::DiskFormat::Raw,
                read_only: true,
            },
        ],
        shared_dir: attempt.layout.shared_dir(),
        container_image_config: Default::default(),
        boot: Some(boot.dependency),
    });
    state.checkpoint_paths = Some(LocalPaths {
        boot_disk: boot.path,
        ..saved_paths
    });
    runtime.box_manager.add_box(&config, &state).unwrap();
    attempt.armed = false;
    drop(attempt);
    let loaded = runtime.box_manager.update_box(&id).unwrap();
    assert_eq!(loaded.checkpoint_paths.unwrap().boot_disk, boot_path);
    let storage = loaded.restored_storage.unwrap();
    assert_eq!(storage.guest_disk().unwrap().path, boot_path);
    std::fs::write(&boot_path, [30; 4096]).unwrap();
    assert!(
        storage.guest_disk().is_err(),
        "cold boot must revalidate the private copy"
    );
}

#[test]
fn checkpoint_explicit_boot_rejects_bad_inputs_without_saved_path_fallback() {
    let home = tempfile::tempdir().unwrap();
    let runtime = runtime(home.path());
    let fixture = Fixture::new();
    for case in [
        "missing",
        "length",
        "digest",
        "directory",
        "symlink",
        "fifo",
    ] {
        let input = home.path().join(case);
        match case {
            "missing" => {}
            "length" => std::fs::write(&input, [29; 4095]).unwrap(),
            "digest" => std::fs::write(&input, [30; 4096]).unwrap(),
            "directory" => std::fs::create_dir(&input).unwrap(),
            "symlink" => std::os::unix::fs::symlink(fixture.boot.path(), &input).unwrap(),
            "fifo" => {
                let name = std::ffi::CString::new(input.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: name is a live NUL-terminated path in the test directory.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let attempt = attempt(&runtime);
        let root = attempt.layout.root().to_path_buf();
        let error = prepare_disks(
            attempt,
            fixture.open(),
            &fixture.paths(),
            Some(&input),
            &CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, BoxliteError::Storage(_)), "{case}: {error}");
        assert!(!root.exists(), "failed attempt must clean up: {case}");
        assert!(fixture.boot.path().exists());
    }
}

#[test]
fn checkpoint_legacy_boot_keeps_saved_path_and_cache_pins_and_explicit_requires_v2() {
    let home = tempfile::tempdir().unwrap();
    let runtime = runtime(home.path());
    let fixture = Fixture::new();
    let pair = fixture.open();
    let mut paths = fixture.paths();
    paths.version = 1;
    let boot = BootDisk::open(&runtime, &pair.dependencies, &paths, None).unwrap();
    assert_eq!(boot.path, fixture.boot.path().canonicalize().unwrap());
    assert!(matches!(
        BootDisk::open(
            &runtime,
            &pair.dependencies,
            &paths,
            Some(fixture.boot.path())
        ),
        Err(BoxliteError::Unsupported(_))
    ));
    let cached = home.path().join("cached.ext4");
    std::fs::copy(fixture.boot.path(), &cached).unwrap();
    let pin = crate::runtime::id::BaseDiskIDMint::mint();
    runtime
        .base_disk_mgr
        .store()
        .insert(&crate::disk::base_disk::BaseDisk {
            id: pin.clone(),
            source_box_id: "__global__".into(),
            name: None,
            kind: crate::disk::BaseDiskKind::Rootfs,
            disk_info: crate::disk::DiskInfo {
                base_path: cached.to_string_lossy().into_owned(),
                container_disk_bytes: 0,
                size_bytes: 4096,
            },
            created_at: 0,
        })
        .unwrap();
    paths.boot_disk = home.path().join("absent-source.ext4");
    let boot = BootDisk::open(&runtime, &pair.dependencies, &paths, None).unwrap();
    assert_eq!(boot.path, cached.canonicalize().unwrap());
    assert_eq!(boot.dependency.base_id, Some(pin));
    paths.version = 2;
    let explicit = home.path().join("missing-explicit.ext4");
    assert!(BootDisk::open(&runtime, &pair.dependencies, &paths, Some(&explicit)).is_err());
    let (attempt, _, boot) = prepare_disks(
        attempt(&runtime),
        fixture.open(),
        &paths,
        None,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(boot.path, cached.canonicalize().unwrap());
    assert!(!attempt.layout.disks_dir().join("boot.ext4").exists());
}

#[test]
fn checkpoint_boot_copy_pins_input_rechecks_content_and_never_overwrites() {
    use std::os::unix::fs::FileExt;
    let home = tempfile::tempdir().unwrap();
    let runtime = runtime(home.path());
    for case in [
        "renamed",
        "digest",
        "short",
        "growing",
        "conflict",
        "cancelled",
    ] {
        let fixture = Fixture::new();
        let mut boot = BootDisk::open(
            &runtime,
            &fixture.open().dependencies,
            &fixture.paths(),
            Some(fixture.boot.path()),
        )
        .unwrap();
        let output = home.path().join(case);
        let cancellation = CancellationToken::new();
        match case {
            "renamed" => {
                std::fs::rename(fixture.boot.path(), home.path().join("pinned-original")).unwrap();
                std::fs::write(fixture.boot.path(), [30; 4096]).unwrap();
            }
            "digest" => fixture.boot.as_file().write_all_at(&[30], 0).unwrap(),
            "short" => fixture.boot.as_file().set_len(4095).unwrap(),
            "growing" => fixture.boot.as_file().set_len(4097).unwrap(),
            "conflict" => std::fs::write(&output, b"keep").unwrap(),
            "cancelled" => cancellation.cancel(),
            _ => unreachable!(),
        }
        let result = boot.copy_private(output.clone(), &cancellation);
        if case == "renamed" {
            result.unwrap();
            assert_eq!(std::fs::read(&output).unwrap(), [29; 4096]);
        } else {
            assert!(result.is_err(), "{case}");
            if case == "conflict" {
                assert_eq!(std::fs::read(&output).unwrap(), b"keep");
            }
        }
    }
}

#[tokio::test]
async fn checkpoint_disk_preparation_failure_and_detached_worker_cleanup_are_owned() {
    let home = tempfile::tempdir().unwrap();
    let runtime = runtime(home.path());
    let fixture = Fixture::new();
    let attempt = attempt(&runtime);
    let root = attempt.layout.root().to_path_buf();
    let unrelated = home.path().join("unrelated");
    std::fs::write(&unrelated, b"keep").unwrap();
    // The private boot destination cannot be created, after QCOW2 has copied.
    std::fs::create_dir(attempt.layout.disks_dir().join("boot.ext4")).unwrap();
    assert!(
        prepare_disks(
            attempt,
            fixture.open(),
            &fixture.paths(),
            Some(fixture.boot.path()),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(!root.exists());
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");

    let attempt = self::attempt(&runtime);
    let root = attempt.layout.root().to_path_buf();
    let paths = fixture.paths();
    let pair = fixture.open();
    let boot = fixture.boot.path().to_path_buf();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            let result = prepare_disks(attempt, pair, &paths, Some(&boot), &worker_cancellation);
            assert!(matches!(result, Err(BoxliteError::Stopped(_))));
            done_tx.send(()).unwrap();
        })
        .await
        .unwrap();
    });
    started_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    cancellation.cancel();
    assert!(root.exists(), "the worker still owns the output directory");
    finish_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!root.exists());
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
}

#[cfg(feature = "rest")]
#[tokio::test]
async fn checkpoint_explicit_boot_rest_backend_returns_unsupported_before_io() {
    let runtime =
        crate::BoxliteRuntime::rest(crate::BoxliteRestOptions::new("http://127.0.0.1:1")).unwrap();
    let result = runtime
        .import_checkpoint_with_boot_disk(
            "absent-pair",
            "absent-boot",
            CheckpointImportOptions::default(),
        )
        .await;
    assert!(matches!(result, Err(BoxliteError::Unsupported(_))));
}

#[test]
fn checkpoint_target_paths_rebuild_normal_dirs_without_fabricating_internal_files() {
    let home = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let layout = BoxFilesystemLayout::new(
        home.path().join("box"),
        crate::runtime::layout::FsLayoutConfig::without_bind_mount(),
        false,
    );
    std::fs::create_dir_all(layout.shared_dir()).unwrap();
    let id = ContainerID::new();
    let options = crate::BoxOptions {
        volumes: vec![crate::runtime::options::VolumeSpec::bind_mount(
            external.path().to_str().unwrap(),
            "/data",
        )],
        ..Default::default()
    };
    let saved = LocalPaths {
        version: 2,
        shares: Default::default(),
        boot_disk: "/not-regenerated".into(),
    };
    let paths = target_paths(&saved, &options, &id, &layout).unwrap();
    assert_eq!(
        paths.shares["uservol0"].path,
        external.path().canonicalize().unwrap()
    );
    let container =
        boxlite_shared::layout::SharedGuestLayout::new(layout.shared_dir()).container(id.as_str());
    for directory in [
        container.rootfs_dir(),
        container.upper_dir(),
        container.work_dir(),
        container.volume_dir("uservol0"),
    ] {
        assert!(directory.is_dir(), "{}", directory.display());
    }
    assert!(!container.exit_file().exists());
    assert!(!layout.shared_dir().join("unknown-file").exists());
    assert!(!paths.boot_disk.exists());
    let missing = home.path().join("external-not-prepared");
    let missing_options = crate::BoxOptions {
        volumes: vec![crate::runtime::options::VolumeSpec::bind_mount(
            missing.to_str().unwrap(),
            "/data",
        )],
        ..Default::default()
    };
    assert!(target_paths(&saved, &missing_options, &id, &layout).is_err());
    assert!(!missing.exists(), "import must never prepare external data");
}
