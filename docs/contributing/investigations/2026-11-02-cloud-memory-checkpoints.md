# Cloud memory checkpoint design

## TL;DR

Store memory checkpoints as organization resources with verified boot dependencies. Restore a new Paused Box on a compatible Runner, including another region, and reconnect original volumes. Preserve the local two-file format and require explicit resume.

2026-11-02 · Proposed contract against `70e908cd`. Documentation only: cloud
operations below remain unavailable and disabled until implementation and
acceptance are complete.

## Scope and acceptance

REST, CLI and Rust/Python/Node/C/Go gain create, upload, list, get, download,
remove and create_box. Reuse disk snapshot infrastructure with separate checkpoint
resources, jobs and storage prefix. Enable only after real acceptance.

The caller pauses the source, stops external writers and keeps external data
compatible with saved memory through restoration. Successful save leaves the
source Paused; restore publishes a new Paused Box requiring explicit resume.
Known save failures preserve Paused. Uncertain native outcomes retain stop/join
safety, never successful publication or implicit resume.

Volumes retain original resource references and **current contents**; no history
or rollback. Coordinate other guests writing those volumes. Cross-region restore
requires compatible Linux x86_64/KVM Runners and volume access. Arbitrary CPU,
architecture/runtime-version portability and existing-Box rollback are excluded.
Native custom-kernel, nested-virtualization, secrets and other restrictions remain.
Acceptance requires the same PID, memory counter and open-file state continuing
after restoration and explicit resume on an independent Runner.

## Related work and lessons

Repository citations refer to `70e908cd`. Observations support these proposals:

| Question and original source | Observation → decision; alternative |
| --- | --- |
| Why include boot bytes? [Dependencies](../../../src/boxlite/src/litebox/checkpoint/pair.rs#L54), [boot selection](../../../src/boxlite/src/runtime/checkpoint_import.rs#L105) | Exact boot bytes are external. Include them; pair-only transfer cannot satisfy an independent Runner. |
| Can transport/catalog/jobs be reused? [Transport](../../../src/boxlite/src/checkpoint_transport.rs#L10), [entity](../../../apps/api/src/box/entities/box-snapshot.entity.ts#L12), [executor](../../../apps/runner/pkg/runner/v2/executor/snapshot.go#L17) | Transport accepts exactly two members; disk jobs assume Stopped and replay disk replacement. Reuse staging/admission/heartbeat patterns with separate cloud codec/resources. Keep direct transfers unchanged. No change leaves no durable cloud memory resource. |
| Are URI-only operations sufficient? [Runner store](../../../apps/runner/pkg/storage/gcs_archive_store.go#L60), [API deletion](../../../apps/api/src/box/stores/snapshot-archive.store.ts#L47), [GCS precondition criteria](https://docs.cloud.google.com/storage/docs/request-preconditions#precondition_criteria) | Existing operations omit generation. Adopt conditional creation and version-specific reads/deletes; reject blind overwrite/delete retry semantics. |
| Are paths/directory tests portable? [Target roots](../../../src/boxlite/src/runtime/checkpoint_import.rs#L42), [Runner mounts](../../../apps/runner/pkg/boxlite/volumes.go#L68), [GCS FUSE limitations](https://docs.cloud.google.com/storage/docs/cloud-storage-fuse/overview#limitations) | Roots are host-specific; object FUSE lacks full POSIX semantics. Bind authorized volume IDs by share tag; require real mounted-volume acceptance. |
| Is Paused already preserved? [Runner mapping](../../../apps/runner/pkg/boxlite/client.go#L708), [API states](../../../apps/api/src/box/enums/box-state.enum.ts#L8) | Paused becomes Started. Introduce Paused actual/desired states before capture; preserve compute reservation and prohibit implicit resume. |
| Can uploaded options be trusted? [Saved application](../../../src/boxlite/src/litebox/checkpoint/pair.rs#L81), [untrusted archive policy](../../../src/boxlite/src/runtime/options.rs#L1515) | Application carries host/security options; ordinary sanitization is insufficient. Apply an authorized cloud import policy, following the disk import boundary. |

## How it works

### Artifact and bindings

Version-1 cloud package: tar+zstd with exactly four regular top-level members:
`manifest.json`, `snapshot.mem`, `snapshot.qcow2`, `boot.ext4`. Preserve native
version-9 pair bytes and require path metadata v2. Boot bytes must match native
length/SHA-256; never rebuild a substitute. Wrapper version does not imply
compatibility with every native version-9 image.

| Manifest field | Contract |
| --- | --- |
| `format`, `version` | `boxlite-memory-checkpoint`, integer `1`; reject unsupported versions/required features. |
| `files` | Three payload names, lengths and lowercase SHA-256; verify native envelope, disk and boot agreement too. |
| `compatibility` | Preserve the native compatibility object and native/application/path schema versions. Runtime/VMM identity collection and Runner compatibility decisions are deferred; do not invent a producer profile. |
| `resources` | Preserve saved allocations and native CPU/RAM/disk sizes and ceilings; cannot relax native checks. |
| `volumes` | Each external volume share: stable `share_tag`, original `volume_id`, normalized `subpath`, guest `mount_path`, `read_only`; exact coverage, unique tags, matching native mount attributes. |
| `source` | Optional Box ID/capture region; uploaded provenance is informational. Authenticated control plane owns organization, resource ID and storage placement. |

Bound manifest to 16 MiB; enforce configured compressed/expanded and per-file
limits with checked arithmetic, independent of untrusted declarations. Reject
duplicate JSON keys, missing/extra/duplicate archive entries, traversal, links,
special files, tar extension entries, truncation and hidden trailers. Checksums detect
corruption, not authority. Use private staging, bounded buffers, verification,
fsync and atomic no-overwrite publication; remove only attempt-owned temporaries.

CLI/SDK upload accepts either a completed package or a local pair with an explicit
exact boot disk (`--boot-disk`) and repeatable `--volume TAG=VOLUME_ID` mappings
(equivalent SDK options). Prepare/verify locally before creating an upload record.
Never infer volume identity from a path. Server independently validates mappings
and organization ownership. Unmapped/cache/other unsupported shares fail explicitly.

Restore accepts a verified boot disk and target roots keyed by saved share tag.
Runner mounts original volumes, confines subpaths, checks guest target/access and
supplies prepared roots. Saved source host paths are nonauthoritative in this
mode; rebuild internal roots using native recipes. Persist boot/root ownership for
Box lifetime and cold starts. Old path-only import keeps its behavior. Missing
objects or incompatible mounts fail; fabricate no files or replacement volumes.

Cloud import validates all saved host settings against server policy: sandbox,
jailer, network/outbound rules, host ports and automatic lifecycle. They cannot
grant host access or disable isolation. Derive authorized host bindings; reject
guest-visible requirements incompatible with policy. Preserve CPU/memory/disk and
guest configuration. Apply policy to restore inputs and persisted cold-start
configuration without rewriting native pair bytes.

### State, ownership and replay

`CheckpointInfo`: ID, name, nullable source Box ID, region, status, nullable
compressed size, dependency/compatibility summary, timestamps and nullable error.
Do not expose host paths, object locations/generations or credentials. Names use
the snapshot 1–128 ASCII alphanumeric/dot/underscore/hyphen rule, starting
alphanumeric, unique per organization outside `deleting`. Failed names stay taken
until removal. Ready resources survive source deletion; volumes remain independent.

States: capture `creating → ready | failed`; upload `uploading → validating →
ready | failed`; removal `ready | failed → deleting → absent`. Only Ready permits
read/restore. Expired uploads and stale jobs become Failed before cleanup.

Persist attempt identity before writing an attempt-specific object with
`ifGenerationMatch=0`. After successful close, record generation, compressed
length and SHA-256. Lost responses reconcile only that attempt's verified object;
never overwrite/adopt another attempt. Transactional Ready settlement checks
resource/job/attempt ownership. Reads/deletes select the recorded generation;
never fall back to latest. Reap orphan attempts, including lost completions, after
writers stop. No database transaction spans network I/O.

Read admission and deletion lock the same row. Deleting blocks new readers;
durable download/restore pins delay physical removal until readers/workers finish.
Disconnect releases a download pin after storage-reader close; restore pins require
native completion/cleanup. Reconcile workers before expiry, not timeout alone.
Delete exact object version before row; cleanup retries remain version-specific.

Capture reserves the source against conflicting lifecycle/storage operations and
rechecks authoritative Paused state. Emergency stop invalidates work and joins
writers. Restore reserves a new Box ID/quota before dispatch; use that ID as stable
local name with persisted checkpoint/job ownership. Replays return/reconcile the
owned result, never overwrite an unrelated or already resumed Box. Cleanup waits
for native work, not client cancellation.

### Public operations

Use normal `/v1` and configured prefix, organization-scoped lookups (foreign IDs
return 404), existing read/write/delete Box permissions and fresh volume access
checks on restore. Only restored name and target region may be overridden.

| Operation | Proposed HTTP contract |
| --- | --- |
| `create` | `POST /boxes/{id}/checkpoints`, optional name; 201 `creating`; source already Paused. |
| `upload` | `POST /checkpoints`, optional name/region; 201 `uploading`. `PUT /checkpoints/{id}/archive` streams package; 202 `validating` after complete receipt. Disconnect/bad package never publishes Ready or creates a Box. |
| `list/get` | `GET /checkpoints` (optional source filter) / `GET /checkpoints/{id}`; 200. Bounded `checkpoints`/`next_cursor` pages; source Box need not exist. |
| `download` | `GET /checkpoints/{id}/archive`; 200 fixed-version bytes, length and archive SHA-256. Client verifies/atomically publishes a new local file. No capture. |
| `remove` | `DELETE /checkpoints/{id}`; 204 queued/repeated owned `deleting` row; 409 while creating/uploading/validating; 404 absent/foreign. |
| `create_box` | `POST /checkpoints/{id}/boxes`, optional name/region; 201 restoring Box. Poll normal Box get for Paused/Error; default region is checkpoint region. |

Non-Ready/conflicting use returns 409; invalid packages/overrides 400; unsupported
capability/compatibility explicit Unsupported; normal authentication/quota mappings
remain. Never automatically retry resource creation after a lost response; inspect
server state first. Internal job replay uses persisted identities separately.

Discovery adds distinct `cloud_checkpoints_enabled`; `checkpoints_enabled` keeps
its direct-transfer meaning. Missing/false means Unsupported, no local fallback.
Pause retains host-writer acknowledgment. Paused reserves CPU/RAM/disk quota,
avoids automatic idle stop/recovery and rejects implicit guest starts (including
exec/preview). Reconnection never resumes; lost Runner cannot silently cold-start.

Runner advertises package/schema/features, runtime/VMM profile, CPU/KVM and resource
support. Initially require a tested matching profile/CPU class; architecture or
version alone is insufficient. Old/unknown Runners get no tasks. Native restore
checks again before execution. Cross-region scheduling also verifies volume access
and object reachability.

CLI gains `checkpoint create/upload/download/ls/get/rm/restore`; preserve local
export/inspect/import and add explicit cloud-package recognition to inspect/import.
Every SDK exposes seven resource operations. C/Go first gain pause/resume and
local export/inspect/import with bindings for Runner; Python sync wrappers and
Node exports follow their native/async surfaces.

Example: pause `web` after stopping external writers, create `cp1`, wait for Ready,
download it, restore `web-copy` in a compatible second region, then explicitly
resume. It reads the original volume's current data. Uploading the download
creates a catalog resource without a Box.

## Validation and implementation gates

### Stage 2a: local package encoding

Implement only `checkpoint_package::pack(pair_directory, boot_disk, options,
limits) -> File` in this commit. The returned anonymous temporary file is complete
and rewound. No catalog, network, mount service, VM operation, publication or
restore compatibility decision runs. Stage 2b will add strict decoding and the
temporary-directory-owning `VerifiedPackage`; it is not part of 2a acceptance.

Research for this slice, checked against the working tree:

| Question and source | Observation and decision |
| --- | --- |
| Can the pair verifier be reused? [`pair::open`, `inspect_memory`, `verify_disk`](../../../src/boxlite/src/litebox/checkpoint/pair.rs) | It pins both child descriptors, bounds native metadata and verifies memory/QCOW2, but verifies the source before copying. Separate descriptor opening from verification internally; copy pinned descriptors into private staging, then run the existing verifier on the copies. Rechecking a source path would allow replacement to redirect the archive. Keep the direct transport's public contract unchanged. |
| Where do volume tags and attributes come from? [`resolve_user_volumes`](../../../src/boxlite/src/litebox/init/types.rs#L50), [`target_paths`](../../../src/boxlite/src/runtime/checkpoint_import.rs#L42), [`vmm_spawn`](../../../src/boxlite/src/litebox/init/tasks/vmm_spawn.rs#L225) | Saved `options.volumes` order defines `uservol{index}`; guest target and access come from those options. The resolver opens source paths, so it cannot be reused here. Match tags and access against native shares without resolving host paths. Exclude only the known internal shared root and VM-owned boot root; reject unsupported external shares and typed/cache mounts. |
| How is a bounded archive finished? [tar `append` and `into_inner`](https://docs.rs/tar/0.4.44/tar/struct.Builder.html#method.append), [zstd 0.13.3 encoder source](https://docs.rs/zstd/0.13.3/src/zstd/stream/write/mod.rs.html), [temporary-file ownership](https://docs.rs/tempfile/3.27.0/tempfile/fn.tempfile.html#resource-leaking) | Write explicit USTAR headers for four fixed short names, finish tar and zstd explicitly, propagate errors, sync and rewind the file. Use counted writers above and below compression; count headers, padding and the footer. Anonymous output is removed when its last descriptor closes. Appending source paths or returning an unfinished encoder would weaken this contract. |
| How will decoding be constrained? [zstd `single_frame` and `window_log_max`](https://docs.rs/zstd/0.13.3/zstd/stream/read/struct.Decoder.html#method.single_frame) | The decoder normally concatenates frames. Stage 2b must stop after one frame, check buffered and remaining input, limit the window to 64 MiB, and count all expanded bytes. A default decoder plus tar iteration alone cannot enforce the proposed package boundary. |

The archive order is `manifest.json`, `snapshot.mem`, `snapshot.qcow2`,
`boot.ext4`. The manifest uses format `boxlite-memory-checkpoint`, version 1,
exact payload-name keys with length and lowercase SHA-256, native compatibility,
schema versions, saved/native resources, volume references and optional source
Box ID/region. It contains no newly collected runtime identity. Native bytes retain
their saved source paths; encoding never opens those saved paths.

Payload size fields may use tar's base-256 numeric encoding, which tar 0.4.45
selects at 8 GiB; imposing octal-only USTAR sizes would exclude large memory/disk
images. This is permitted within an ordinary file header. GNU/PAX extension
entries remain forbidden. Stage 2b must accept this numeric encoding with checked
size parsing and the same caller limits.

Open the pair directory and all three input files once with no-follow regular-file
checks. Copy using fixed buffers and explicit per-file bounds into an attempt-owned
private directory with a separate `pair/` child. Concurrent source modification may
make verification fail; a successful archive always contains exactly the staged,
verified bytes. Verify path metadata v2, the native pair and saved resource
allocation, then compare staged boot length and SHA-256 with native boot metadata.
Do not change either original pair file.

Options carry a list of `(share_tag, volume_id, optional subpath)` mappings and
optional informational provenance. Require a nonempty volume ID, unique tags and
exact coverage of saved user-volume shares. Derive guest mount paths and read-only
attributes from saved options; reject conflicting native share access, missing or
extra shares/mappings, and unsupported mount origins. Subpaths must already be
normalized relative slash-separated paths: reject empty values, absolute paths,
`.`/`..`, repeated/trailing separators, backslashes and NULs. No organization
authorization or source-directory existence check is implied by this validation.

Require explicit compressed, expanded, memory, QCOW2 and boot byte limits, with no
unlimited default. Check arithmetic, enforce copy bounds before staging bytes, cap
serialized manifest output at 16 MiB, and enforce archive bounds while writing.
Use zstd level 1, a checksum and a window no greater than 64 MiB. RAII removes
attempt-owned staging and output on every error; no named destination is published.
This blocking file API must run outside an async executor thread, as with the
existing transport codec.

Validation for 2a uses the production pack entry point: inspect member order and
manifest digests, compare all three decoded payloads byte-for-byte, reject wrong
boot size/hash, malformed/corrupt pairs and v1 path metadata, exercise volume
coverage/access/subpath conflicts, and check exact size boundaries and overflow.
Read/write fault injection must exercise codec copy/encoding helpers; a filesystem
failure must return an error without publishing output. These are new-feature
tests, not a claim that a preexisting pack API exhibited a bug. Stage 2b owns
round trips through the production unpack entry point and hostile archive tests.

Run the scoped `checkpoint_package`, `checkpoint_pair` and
`checkpoint_transport` Rust make suites, Rust formatting and Rust lint with
dependency stubs where specified. Report environmental failures as blocked checks.
No KVM, mounted-volume or cross-Runner restoration claim follows from these tests.

| Claim/risk | Planned check and required result |
| --- | --- |
| Integrity/bounds | Round-trip production artifacts; corrupt/omit/duplicate/traverse/overflow, exhaust disk, interrupt transfers: no published destination or Ready resource. |
| Bindings/trust | Change runtime home/remove source boot; exact bundle works, wrong boot fails. Real subpaths/read-only modes, denied/foreign volumes, malicious host paths/security/network/ports cannot bypass policy. |
| Replay/cleanup | Lose responses, restart Runner, redeliver tasks; protect other generations and existing/resumed Boxes. Race delete with download/restore; wait for readers/writers. |
| Process/storage continuity | Real Linux x86_64/KVM and production sandbox, independent Runners: same PID/memory/open-file state after explicit resume; actual GCS/S3 mounts, including missing/denied dependencies. Directory fakes do not qualify. |
| Regression | Disk snapshots, two-file local import/export, direct `serve` HTTP transfers, all SDK contracts and missing capabilities stay compatible. |

One validated commit at a time: codec → bindings → C native → Go native → cloud
Paused lifecycle → disabled catalog → Runner capture → upload → download →
compatible scheduling → create Box → Rust → CLI → Python → Node → C cloud → Go
cloud → real acceptance/enablement. Each slice updates owning docs and uses `make`;
split further for reviewability.

Observed: repository sources and both Google originals read; no VM/cloud/mounted
volume acceptance run here. Libkrun submodule is absent: inspect its CPU/KVM
validators and define/test the profile before compatibility extraction. An
unestablished profile blocks scheduling enablement. Real GCS generation behavior
and object-FUSE restore remain acceptance gates, not proven capabilities.
