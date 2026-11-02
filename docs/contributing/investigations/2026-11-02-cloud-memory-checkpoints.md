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
| `compatibility` | Producer runtime/VMM build identities, schema/features, Linux/x86_64/KVM and CPU/KVM requirements, allocations/ceilings; cannot relax native checks. |
| `volumes` | Each external volume share: stable `share_tag`, original `volume_id`, normalized `subpath`, guest `mount_path`, `read_only`; exact coverage, unique tags, matching native mount attributes. |
| `source` | Optional Box ID/capture region; uploaded provenance is informational. Authenticated control plane owns organization, resource ID and storage placement. |

Bound manifest to 16 MiB; enforce configured compressed/expanded and per-file
limits with checked arithmetic, independent of untrusted declarations. Reject
duplicate JSON keys, missing/extra/duplicate archive entries, traversal, links,
special files, tar extensions, truncation and hidden trailers. Checksums detect
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
