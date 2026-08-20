# TMT Multi-Device ESP Test Debug Session State

## Current Status

Root cause of the actual test failure has been found and fixed (commit `831f5f91`).
CI run for that fix is in flight; see "Next Step" below.

## Root Cause History (in the order they were found)

1. **`--target-transport containers-storage`** on `bcvk libvirt run` — fixes
   `bootc image copy-to-storage` reading from ostree instead of pulling from
   `docker://localhost` (connection refused loop).
2. **Removed `prepare: how: install`** from the plan — packages (lvm2, dosfstools,
   e2fsprogs) are already in centos-bootc:stream9; the prepare step was triggering
   tmt's bootc package manager, causing a connection-refused loop.
3. **Added `rsync`** to `provision-derived.sh` — tmt needs rsync on the guest to
   sync test files.
4. **`--pull never`** on `podman run` in `run_install` — prevents a podman retry
   loop trying to reach a non-existent registry.
5. **Removed `exec` from the tmt plan's `script:`** (`6c35f57d`) — in theory lets
   tmt capture nushell's stdout into `output.txt`. This *alone* did not fix
   anything (see next point) but is harmless/correct to keep.
6. **Nushell stdout buffering** (`0d9f636f`): nushell buffers stdout internally
   and loses everything printed so far when the script exits abruptly on an
   uncaught `error make`. This is why `output.txt` kept coming back with only
   the SSH "permanently added" banner and none of the test's own `print` output,
   even after fix #5. Fixed by adding a `log` helper that mirrors every `print`
   to a plain file (`/var/tmp/bootc-esp-test.log`) via `save --append`, which is
   a synchronous file write and survives regardless of how the nu process exits.
   `ci.yml` now `scp`s that file off the guest right after the `tmt run` command
   finishes (while the VM is still alive — the `trap ... EXIT` in that step
   destroys the VM as soon as the step ends) and surfaces it in the
   `Show TMT results` annotations and in the `tmt-logs-multi-device-esp` artifact.
7. **Nushell parse error** (also `0d9f636f`): wrapping the `podman run` invocation
   as `do { podman run ... --pull never ... } | complete` (added in `c4a71b24` to
   capture output) actually fails to *parse* in nushell — a flag followed by a
   bare word (`--pull never`) confuses the parser when the multi-line external
   command isn't itself wrapped in parens inside a bare `do {}` block ("expected
   operator" at `never`). This was silently swallowing the whole test with a
   generic parse error and had never been noticed. Fixed by wrapping the
   external command in its own parens inside the `do` block:
   `do { (podman run ...) } | complete`.
8. **Real root cause of the test failure** (`831f5f91`), found once fixes #6/#7
   above finally surfaced the actual output: `bootc install to-existing-root`
   was failing with:
   ```
   ERROR Installing to filesystem: Verifying fetch: Creating importer: failed to
   invoke method OpenImage: failed to invoke method OpenImage: fetching manifest
   latest in localhost/bootc: pinging container registry localhost: Get
   "https://localhost/v2/": dial tcp [::1]:443: connect: connection refused
   ```
   `bootc install`'s *target* image reference (the image future upgrades will
   pull from — distinct from the *source* it installs from) defaults to the
   `registry` transport (see `InstallTargetOpts::target_transport` in
   `lib/src/install.rs`), so a bare `localhost/bootc` name gets parsed as "pull
   from a registry named localhost" and bootc tries to fetch it over the
   network to verify it (`verify_target_fetch`). The image only exists in local
   podman/containers-storage (put there by `bootc image copy-to-storage` earlier
   in the test), so the fix is to pass `--target-transport containers-storage`
   to `bootc install to-existing-root` in `run_install`, matching how the image
   actually got there.

## Next Step

Commit `831f5f91` (target-transport fix) has been pushed to `ckyrouac/main` and
`ckyrouac/test-backport-multi-device`. Watch the triggered CI run's
`TMT: multi-device ESP` job:

```bash
gh run list --repo ckyrouac/bootc --branch main --limit 5
gh run view <run-id> --repo ckyrouac/bootc
# once the TMT job finishes, get its debug log either via the artifact:
gh run download <run-id> --repo ckyrouac/bootc -n tmt-logs-multi-device-esp -D /tmp/tmt-artifact
find /tmp/tmt-artifact -iname output.txt -o -iname bootc-esp-test.log
# or via annotations:
gh api /repos/ckyrouac/bootc/check-runs/<tmt-job-id>/annotations
```

If `test_single_esp` now passes, watch for the remaining four scenarios
(`test_dual_esp`, `test_three_devices_partial_esp`, `test_single_device_no_lvm`,
`test_no_esp_failure`) — they use the same `run_install` helper so should
benefit from the same fix, but each exercises different multi-device/ESP
detection code paths in the `blockdev`/`install` backport and could still
uncover real bugs in the backported logic itself (which is the actual point of
this test).

Note: the `output.txt (...)` and `bootc-esp-test.log` GitHub Actions
`::error::` annotations in `Show TMT results` can come back truncated/garbled
because the captured text contains ANSI colour escape codes and raw newlines
that aren't fully escaped for the `::error::` workflow-command format. When
that happens, prefer downloading the `tmt-logs-multi-device-esp` artifact
(always uploaded, see "Archive TMT logs" step) and reading the files directly
— that has reliably contained the full, correct output even when the
annotation was garbled.

## Branch State

- **Branch**: `test-backport-multi-device` (also mirrored onto `main` on the
  `ckyrouac` fork, since `ci.yml` only triggers on push to `main` or PRs — there
  is no open PR for this branch, so pushing to fork `main` is what triggers CI)
- **Local HEAD**: `831f5f91` (pushed to `ckyrouac/main` and
  `ckyrouac/test-backport-multi-device`)

## Key Files

### Plan
`plans/test-32-multi-device-esp.fmf`:
```yaml
provision:
  how: connect
  guest: localhost
  user: root
summary: Test multi-device ESP detection for to-existing-root
execute:
  how: tmt
  script: nu tests/booted/test-multi-device-esp.nu
```

### Test Script
`tests/booted/test-multi-device-esp.nu`

- `log` helper (near top of file): mirrors `print` to `/var/tmp/bootc-esp-test.log`.
- `run_install`: runs `podman run ... bootc install to-existing-root ...`,
  captures stdout/stderr via `complete`, logs them, and raises with the
  captured stderr on unexpected failure (`expect_failure` param lets
  `test_no_esp_failure` inspect an expected failure instead).
- `main()`: copies the booted image into podman storage via
  `bootc image copy-to-storage`, then runs the five test scenarios in order.

### Workflow
`.github/workflows/ci.yml`, job `TMT: multi-device ESP`:
- Uses `bootc-dev/actions/bootc-ubuntu-setup@main` with `libvirt: true`.
- Boots the guest with
  `bcvk libvirt run --target-transport containers-storage localhost/bootc`.
- Runs `tmt --context=running_env=image_mode run ... provision --how=connect`.
- Right after the `tmt run` command, `scp`s `/var/tmp/bootc-esp-test.log` off
  the guest (while it's still alive) to the runner at the same path.
- `Show TMT results` step emits `output.txt`, `failures.yaml`,
  `execute/results.yaml`, and `bootc-esp-test.log` as `::error::` annotations.
- `Archive TMT logs` step uploads `/var/tmp/tmt` and `/var/tmp/bootc-esp-test.log`
  as the `tmt-logs-multi-device-esp` artifact (more reliable than annotations,
  see note above).

### Container Image Build
`hack/provision-derived.sh` — installs `nu rsync` in the image.

## How to Read CI Results

```bash
# List recent runs on a branch
gh run list --repo ckyrouac/bootc --branch main --limit 5

# Get the TMT job ID for a run
gh api /repos/ckyrouac/bootc/actions/runs/<RUN_ID>/jobs --jq \
  '.jobs[] | select(.name | contains("TMT")) | .id'

# Get annotations (may be garbled, see note above — prefer the artifact)
gh api /repos/ckyrouac/bootc/check-runs/<JOB_ID>/annotations | python3 -c "
import json,sys
for a in json.load(sys.stdin):
    if a['annotation_level'] != 'warning':
        print(f\"--- {a['title']!r} ---\"); print(a['message'][:6000])
"

# Download the full artifact (most reliable)
gh run download <RUN_ID> --repo ckyrouac/bootc -n tmt-logs-multi-device-esp -D /tmp/tmt-artifact
find /tmp/tmt-artifact -iname output.txt -exec cat {} \;
```

## Context: What This Test Is

The multi-device ESP test validates the `backport: multi-device parent support to v1.1.6`
commit which backports Intel VROC RAID and multipath support from v1.15.x to the v1.1.6
codebase (for rhel-9.6.0 and rhel-10.0 which can't run Rust 1.85+).

The test creates loopback devices with LVM spanning multiple disks, each potentially
having an ESP partition, and verifies that `bootc install to-existing-root` correctly
finds and installs bootloader to all ESPs.

## Test Scenarios

1. Single ESP: one disk has ESP, other just LVM — install should find and use the ESP
2. Dual ESP: both disks have ESP — install should use both
3. Three devices, partial ESP: ESP on disk1 and disk3 only
4. Single device no LVM: simple ESP + root partition, no LVM
5. No ESP: install should FAIL with ESP-related error message

## What the Test Does (run_install)

```nushell
podman run --rm --privileged --pull never
    -v "$mountpoint:/target"
    -v /dev:/dev
    -v /run/udev:/run/udev:ro
    -v /usr/share/empty:/usr/lib/bootc/bound-images.d
    --pid=host
    --security-opt label=type:unconfined_t
    --env BOOTC_BOOTLOADER_DEBUG=1
    localhost/bootc
    bootc install to-existing-root
        --disable-selinux
        --acknowledge-destructive
        --target-no-signature-verification
        --target-transport containers-storage
        /target
```
