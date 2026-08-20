# TMT Multi-Device ESP Test Debug Session State

## Current Status

The TMT multi-device ESP test (`test-32-multi-device-esp`) is running in CI but failing
at the first scenario (`test_single_esp`) with:

```
Error: Single ESP test failed: {msg: External command had a non-zero exit code}
```

The test runs for ~2 minutes before failing. `bootc image copy-to-storage` takes ~1 min,
then `test_single_esp` fails immediately when calling `run_install`.

## Branch State

- **Branch**: `test-backport-multi-device`
- **Remote main**: `02590c7b6ff4` (pushed to ckyrouac/bootc)
- **Local HEAD**: `6c35f57d` (NOT YET PUSHED)

## Commits Ready to Push

```
6c35f57d tmt: remove exec from script - allows tmt to capture nushell stdout
```

This removes `exec` from `script: exec nu tests/...` → `script: nu tests/...` so tmt
can capture nushell's stdout into `output.txt` for diagnostics.

## Root Cause Analysis So Far

### Fixed Issues
1. **`--target-transport containers-storage`** on `bcvk libvirt run` — fixes
   `bootc image copy-to-storage` reading from ostree instead of pulling from
   `docker://localhost` (connection refused loop)
2. **Removed `prepare: how: install`** from the plan — packages (lvm2, dosfstools,
   e2fsprogs) are already in centos-bootc:stream9; prepare step was triggering
   tmt's bootc package manager causing the connection refused loop
3. **Added `rsync`** to `provision-derived.sh` — tmt needs rsync on the guest to
   sync test files
4. **`--pull never`** on `podman run` in `run_install` — prevents podman retry loop

### Current Failing Point
`run_install` in `test_single_esp` fails with non-zero exit. The `podman run --pull never
localhost/bootc bootc install to-existing-root` command fails. Two hypotheses:

1. `localhost/bootc` is NOT in podman storage despite `bootc image copy-to-storage`
   running — perhaps bootc's containers-storage and rootful podman's storage differ
2. `bootc install to-existing-root` itself fails for some reason

### Next Debug Step
Push `6c35f57d` and look at `output.txt` which will now contain nushell's stdout.
The verbose diagnostics in `main()` will show:
- `bootc image copy-to-storage` exit code and output
- Whether `podman image inspect localhost/bootc` succeeds
- Which test scenario fails and why

## Key Files

### Plan
```
/sandbox/bootc/plans/test-32-multi-device-esp.fmf
```
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
`/sandbox/bootc/tests/booted/test-multi-device-esp.nu`

Has verbose diagnostics in `main()`:
- Prints `bootc image copy-to-storage` exit code and output
- Checks `podman image inspect localhost/bootc`
- Prints progress before each scenario

### Workflow
`/sandbox/bootc/.github/workflows/ci.yml`

TMT job:
- Uses `bootc-dev/actions/bootc-ubuntu-setup@main` with `libvirt: true`
- Uses `bcvk libvirt run --target-transport containers-storage localhost/bootc`
- Runs `tmt --context=running_env=image_mode run ... provision --how=connect`
- `Show TMT results` step emits `output.txt`, `failures.yaml`, `execute/results.yaml`
  as `::error::` annotations visible via `rtk gh api check-runs/{id}/annotations`

### Container Image Build
`/sandbox/bootc/hack/provision-derived.sh` — installs `nu rsync` in the image

## How to Read Annotations

```bash
# Get job ID
rtk gh api /repos/ckyrouac/bootc/actions/runs/{RUN_ID}/jobs | python3 -c "
import json,sys; d=json.load(sys.stdin)
for j in d['jobs']:
    if 'TMT' in j['name']: print(j['id'])
"

# Get annotations
rtk gh api /repos/ckyrouac/bootc/check-runs/{JOB_ID}/annotations | python3 -c "
import json,sys; d=json.load(sys.stdin)
for a in d:
    if a['annotation_level'] != 'warning':
        print(f\"--- {a['title']!r} ---\")
        print(a['message'][:2000])
"
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
        /target
```
