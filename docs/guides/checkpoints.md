# Checkpoints: pause, export and resume a running box elsewhere

## TL;DR

On Linux x86_64 with KVM, a box created with `--enable-checkpoint` can be paused, written to one
checkpoint file and imported as a new box whose guest processes continue after `resume`. CLI and
Rust only. Real-KVM acceptance passes on a GitHub-hosted runner; see [Verification status](#verification-status).

```bash
boxlite create --enable-checkpoint --name web -v /srv/data:/data alpine:latest \
  sh -c 'while :; do date >> /data/log; sleep 1; done'
boxlite start web
boxlite pause --host-writers-stopped web     # stop every host writer of /srv/data first
boxlite checkpoint export web web.boxckpt    # the box stays paused
boxlite stop web                             # the file is self-contained; see "Run each checkpoint once"
boxlite checkpoint import --name web2 web.boxckpt   # prints the new box ID; paused
boxlite resume web2                          # the same guest processes continue
```

## Lifecycle

```
create --enable-checkpoint ─▶ running ──pause──▶ paused ──resume──▶ running
                                                  │
                                       checkpoint export  (stays paused)
                                                  ▼
                                         one checkpoint file
                                                  │
                                       checkpoint import  (new box, paused)
                                                  ▼
                                    paused ──resume──▶ same processes continue
```

| Step | What happens | Fails when |
|---|---|---|
| `pause` | Every vCPU and device stops at one consistent cut. No SIGSTOP, no guest filesystem freeze. | The box is not running, or has a volume and `--host-writers-stopped` was not given |
| `checkpoint export BOX FILE` | Writes memory, devices, both disks and every share's files to `FILE`, published atomically. | The box is not paused, `FILE` exists, or its directory does not |
| `checkpoint import FILE` | Creates a new box: new ID and host paths, same guest container, process and execution IDs. Status `paused`. | The file is not a valid checkpoint, or the host cannot run it |
| `resume` | Lets the guest run, reconnects, and checks it is the guest that was paused. | The guest is not the one paused: the VM is then stopped |

While paused, the box refuses `exec`, attach, `cp`, metrics, SSH and network changes; health probes
are skipped and nothing resumes it implicitly. `resume`, `stop` and `checkpoint export` act on it.
Host connections to the guest, including open exec output streams, may end at the pause; the
guest processes keep running after `resume`, and attaching by execution ID reconnects to them.

## Host writers of volumes

Pausing stops the guest, not the host. A host process that writes a volume of a paused box would
change it under the guest and tear the checkpoint.

| Box has | `pause` needs |
|---|---|
| No volumes | nothing |
| Any volume (`-v` host directory or managed volume) | `--host-writers-stopped`: you stopped every host process that writes those directories |

The assertion holds until `boxlite resume` or `boxlite stop`; keep the writers stopped until then.
Directories the box owns itself are coordinated by the runtime.

## What a checkpoint holds and needs

- **Self-contained:** the importing host needs only the file and a compatible BoxLite runtime (its
  bundled kernel and shim). No image, image cache or source box is read. Volumes are restored as
  copies inside the new box's home; the original host paths are not used.
- **Same identity:** import must run as the same user (effective UID/GID) as the export.
- **A restricted CPU.** A checkpoint-enabled guest does not see CPU features whose state a
  checkpoint does not save, so it never enables them:

  | Hidden | CPUID |
  |---|---|
  | Supervisor and dynamic XSAVE state: XSAVES, XFD, every XSAVE component except x87, SSE, AVX, AVX-512 and PKRU | leaf 0xD |
  | SGX, MPX, RDT, processor trace, CET, PKS, architectural LBR, user interrupts, AMX | leaf 7 |
  | Nested VMX/SVM, AMD IBS | leaves 1, 0x80000001 |
  | PMU, RDT, processor trace, SVM, IBS, SEV, PQoS and PerfMonV2 leaves | 0xA, 0xF, 0x10, 0x14, 0x8000000A, 0x8000001B, 0x8000001F, 0x80000020, 0x80000022 |

  The guest also gets no leaf 0x1F; it reads its topology from leaf 1 and 0xB, or AMD's extended
  leaves, which carry each vCPU's own APIC ID. Boxes without `--enable-checkpoint` see the full CPU.
- **A matching host.** Import refuses a host whose KVM-supported CPUID, MSR list, XSAVE size or KVM
  capabilities differ from the exporting host's. Only the APIC IDs of the CPU that queried KVM may
  differ, so a checkpoint imports on the host that wrote it.
- **Size:** the file holds the box's RAM, both disks at their full virtual size and every share's
  files. A checkpoint-enabled box may write files up to twice its disk size plus its RAM plus 4 GiB;
  an export that would not fit fails before writing, and the box stays paused.
- **Shares use DAX.** Every share of a checkpoint-enabled box gets a 512 MiB DAX window and the guest
  mounts it with `dax=always`, so file mappings are part of the checkpoint. A guest kernel without
  FUSE DAX fails the box at start; there is no fallback to a mount without DAX. Boxes without
  `--enable-checkpoint` mount shares without DAX, as before.
- **Shares are archived as the guest sees them.** Every file and directory under the box's shares must
  be:

  | Requirement | Otherwise |
  |---|---|
  | Owned by the user running BoxLite | export fails |
  | Readable (files) or listable and searchable (directories) | export fails |
  | Free of xattrs other than `user.*` and POSIX ACLs naming only that user/group | export fails |
  | A regular file, directory or symlink (no FIFO, socket or device) | export fails |

  `trusted.*` attributes hidden from the unprivileged runtime are not captured. Export never drops
  something it cannot restore; it fails instead.
- **Trusted input.** Import only checkpoints you or a party you trust wrote. The file is checked
  for structure and size limits, not for its origin; it carries the guest's whole memory.

## Run each checkpoint once

A checkpoint is a moment of a running guest. Every copy that runs from it starts with the same
memory, so random-number state, generated IDs, session tokens and keys are repeated in each copy.
BoxLite does not reseed the guest.

| Use | Result |
|---|---|
| Export, `stop` the source, import once, resume | one guest continues |
| Export, then `resume` the source **and** import | two guests share that state |
| Import the same file twice | two guests share that state |

To move a box, stop the source after export and import the file once. Once the source resumes, any
later import of that file repeats state the source already used.

## Support

| | Supported | Refused (`Unsupported`, before the box runs) |
|---|---|---|
| Host | Linux x86_64 with KVM | every other OS or architecture |
| Entry points | Rust `BoxliteRuntime`/`LiteBox`, `boxlite` CLI (embedded runtime) | Python, Node.js, Go and C SDKs (no checkpoint API); REST runtimes and `boxlite serve` |
| Isolation | default sandbox (`--security enable`) | `--security disable`, `advanced.isolate_mounts` |
| Network | gvproxy network or `--network disabled` | TSI networking |
| Options | up to 5 directory volumes, up to 128 vCPUs | custom kernel, nested virtualization, secrets, single-file volumes |
| Old disk operations | — | disk snapshots, `clone_box`, the disk `export` and `restore_from_archive` on a checkpoint-enabled box; importing a legacy `.boxlite` archive whose options enable checkpoints |

Boxes without `--enable-checkpoint` are unchanged.

## Failures

| Failure | Result |
|---|---|
| Export refused or failed with a known outcome (destination exists, disk full, unsupported share content, image larger than the box's file size limit) | Box stays paused; only files this export created are removed; retry or `resume` |
| Import fails at any step | No box, no box directory; files this attempt created are removed |
| The runtime cannot tell whether a pause, resume or export happened (the control channel broke) | The VM is stopped, never retried |
| After resume, the guest is not the paused one | The VM is stopped and `resume` fails |

After `resume` of an imported box, guest network connections that were open at the cut may break;
new connections work.

## Verification status

| Area | Status |
|---|---|
| CLI parsing, refusals and non-KVM runtime paths | unit tests |
| x86_64 guest agent (static musl build) and its unit tests | built with `make guest`. The execution tests (`service::exec::`) pass 69/69 one at a time in an emulated amd64 container. The latest full suite there is incomplete, not a pass: before it was stopped it printed 356 passed, 3 failed (two `service::files` tests that need `openat2`, which the emulator lacks, and one fd test that fails only when tests run in parallel) |
| Real guest continuation (`make test:checkpoint:acceptance`) | passes on KVM: [run 40329148427](https://github.com/lfld287/boxlite/actions/runs/40329148427), commit `47b0fb39`, libkrun `96c0ca3c`, GitHub-hosted `ubuntu-22.04` (AMD EPYC 9V45, kernel 6.8 azure) |
| x86_64 VMM seccomp filter with the checkpoint syscalls | **UNVERIFIED** at runtime: the filter is compiled and its decisions checked; a box with networking skips it (gvproxy runs in the shim), so the acceptance run did not install it |
| virtio-fs DAX mappings in a real Linux guest, and FUSE DAX in the bundled guest kernel | verified by the same run: the share mounts with `dax=always`, the guest reports its mapped file as DAX, and the checkpoint holds the file mapping |

The acceptance test runs one box with a long-lived process holding a heap-only random marker, a
counter, open and unlinked files on the disk and a volume, and a DAX mapping; it pauses, exports,
destroys the box and its home, imports into an empty runtime and requires the same PID, marker and
execution to continue. It needs `/dev/kvm`, `make runtime:debug` and registry access for
`alpine:latest`.

## Run KVM acceptance in a fork

The `ci/checkpoint-kvm` branch carries the **Checkpoint KVM acceptance** workflow. Its libkrun
gitlink is the fixed commit `96c0ca3ce15e71eda59c64555dbfad4297d664e9`, fetched from the
[lfld287/libkrun](https://github.com/lfld287/libkrun) fork, and the workflow fails unless the checkout
is exactly that commit. It adds four KVM fixes on top of the snapshot implementation `f6844597`:

| Commit | Fix |
|---|---|
| `258cdbb2` | The MP table describes all 24 IOAPIC inputs, so disks (GSI 16–22) and the NIC (GSI 23) get IRQs |
| `4fb5baeb` | A checkpoint boot hides the CPU features listed under [What a checkpoint holds and needs](#what-a-checkpoint-holds-and-needs) |
| `80cea934` | Export accepts the hypervisor, TSC-deadline and HTT bits the VMM sets itself |
| `96c0ca3c` | Import compares the hosts' supported CPUID without the calling CPU's APIC IDs |
Pushing this branch runs it on an Ubuntu 22.04 x86_64 GitHub-hosted runner. It checks
KVM access, builds the real runtime and explicitly runs
`make test:checkpoint:acceptance SETUP_DONE=1` after building the test executable.
No dependency stubs are used. Missing KVM, build failures and failed continuation
assertions fail the job; an ordinary unit-test pass is not checkpoint acceptance.

The acceptance step sets two variables, usable locally too:

| Variable | Value in CI | Effect |
|---|---|---|
| `RUST_LOG` | `boxlite=debug,krun=debug` | Debug logs from BoxLite and libkrun (libkrun reads `RUST_LOG` itself). Plain `debug` would also log registry tokens |
| `BOXLITE_CHECKPOINT_LOGS` | `$GITHUB_WORKSPACE/acceptance-logs/boxes` | Absolute directory where the test copies each box's `logs/`, `exit`, `exit.previous` and `shim.stderr` before its home is deleted; disks, memory and keys are never copied |

The uploaded `checkpoint-kvm-logs-<attempt>` artifact holds the host, setup, build, acceptance
and `dmesg` logs and those box logs. A failed import removes its box directory, so the operator
log (the acceptance output) carries the tail of that shim's stderr instead.

To launch it explicitly through the existing **Test** workflow entry point:

```bash
gh workflow run test.yml --repo lfld287/boxlite --ref ci/checkpoint-kvm -f checkpoint_only=true
```

The option skips ordinary test jobs and runs only checkpoint acceptance. Normal
dispatches retain their existing test matrix. Open the resulting **Test** run
(or the push-triggered **Checkpoint KVM acceptance** run) and download
`checkpoint-kvm-logs-<attempt>` for setup, build and acceptance output. To repeat
that exact commit, use **Re-run all jobs** or:

```bash
gh run rerun RUN_ID --repo lfld287/boxlite
```

This branch does not change the snapshot PR head or the fork's default branch.
It uses a branch push because GitHub requires a workflow on the default branch
for [manual dispatch](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow).
GitHub [does not officially support nested virtualization](https://docs.github.com/en/actions/concepts/runners/github-hosted-runners#runner-images);
KVM availability and successful guest continuation must be established by the run.
