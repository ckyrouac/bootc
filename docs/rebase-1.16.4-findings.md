# bootc 1.16.4 Rebase Findings

## Overview

Rebase of bootc to upstream v1.16.4 across all RHEL and CentOS Stream branches.

Upstream release: https://github.com/bootc-dev/bootc/releases/tag/v1.16.4

## Rust Edition 2024 Blocker

bootc v1.14.0 and later require Rust edition 2024, which needs Rust/Cargo 1.85+.
All 13 crates in the workspace declare `edition = "2024"`.

The last version using edition 2021 is **v1.13.0**.

### Rust versions by stream

| Stream | Rust Version | Edition 2024? | Can build 1.16.4? |
|--------|-------------|---------------|-------------------|
| rhel-9.6.0 | 1.84.1 | No | No |
| rhel-9.8.0 | 1.92.0 | Yes | Yes |
| rhel-10.0 | 1.84.1 | No | No |
| rhel-10.2 | 1.92.0 | Yes | Yes |
| c9s | 1.92.0 | Yes | Yes |
| c10s | 1.92.0 | Yes | Yes |

The rhel-10.0 build failure is visible in the Konflux pipeline rpmbuild-x86-64.log:

```
error: failed to load manifest for workspace member `/builddir/build/BUILD/bootc-1.16.4/crates/blockdev`

Caused by:
  feature `edition2024` is required

  The package requires the Cargo feature called `edition2024`, but that feature is
  not stabilized in this version of Cargo (1.84.1 (66221abde 2024-11-19)).
```

## Multi-Device Parent Backport Assessment

The specific feature needed on rhel-9.6.0 and rhel-10.0 is multi-device parent support
(Intel VROC RAID, multipath). The relevant commits from v1.15.0..v1.16.4 are:

| Commit | Description | Files touched |
|--------|-------------|---------------|
| 460c2efa | install: Enable installing to devices with multiple parents | bootloader.rs, install.rs, bwrap.rs, tests |
| 347a6f49 | composefs: Walk parent devices to find ESP partition | bootc_composefs/boot.rs, store/mod.rs |
| 58fa354b | tests: Skip multi-device ESP test on non-UEFI systems | test-multi-device-esp.nu |
| b1fe5d69 | blockdev: Handle ESP discovery on Intel VROC RAID devices | blockdev.rs, test fixtures |
| bd93a1e5 | tmt: Rename multi-device-esp test file | test fmf rename |
| 9c3e439f | blockdev: Restore multipath partition number fallback | blockdev.rs |

### Why backport is difficult

1. **Path restructuring**: v1.1.6 uses `blockdev/`, `lib/`, `utils/` while v1.16.4 uses
   `crates/blockdev/`, `crates/lib/`, `crates/utils/`. Every commit hits modify/delete
   conflicts.

2. **Massive code divergence**: Files have grown significantly between versions:
   - blockdev.rs: 392 -> 1003 lines
   - bootloader.rs: 137 -> 461 lines
   - install.rs: 2005 -> 3182 lines

3. **composefs dependency**: Commit 347a6f49 touches `crates/lib/src/bootc_composefs/boot.rs`
   which does not exist in v1.1.6. The composefs backend is a v1.14+ feature.

4. **No clean cherry-picks**: All 6 commits conflict when cherry-picked onto v1.1.6.
   The backport would require manually reimplementing the feature against the old codebase.

### Options for rhel-9.6.0 and rhel-10.0

1. Leave on current version (1.1.6) — no multi-device support
2. Rebase to v1.13.0 — last edition 2021 version, but still a large jump and may not
   include the multi-device commits (they landed in v1.15.1)
3. Manual backport — rewrite the multi-device patches for the v1.1.6 codebase (~week+ effort)
4. Get Rust 1.85+ into those streams — unlikely for z-streams

## JIRA Tickets and MRs

### RHEL dist-git

| Stream | JIRA | Scratch Build | MR | Status |
|--------|------|---------------|-----|--------|
| rhel-9.6.0 | RHEL-213794 | [71337665](https://brewweb.engineering.redhat.com/brew/taskinfo?taskID=71337665) | [!24](https://gitlab.com/redhat/rhel/rpms/bootc/-/merge_requests/24) | Rust too old |
| rhel-9.8.0 | RHEL-213807 | [71338466](https://brewweb.engineering.redhat.com/brew/taskinfo?taskID=71338466) | [!25](https://gitlab.com/redhat/rhel/rpms/bootc/-/merge_requests/25) | OK |
| rhel-10.0 | RHEL-213808 | [71338506](https://brewweb.engineering.redhat.com/brew/taskinfo?taskID=71338506) | [!26](https://gitlab.com/redhat/rhel/rpms/bootc/-/merge_requests/26) | Rust too old - build failed |
| rhel-10.2 | RHEL-213809 | [71338509](https://brewweb.engineering.redhat.com/brew/taskinfo?taskID=71338509) | [!27](https://gitlab.com/redhat/rhel/rpms/bootc/-/merge_requests/27) | OK |

### CentOS Stream dist-git

| Stream | JIRA | MR | Status |
|--------|------|-----|--------|
| c9s | RHEL-213824 | [!98](https://gitlab.com/redhat/centos-stream/rpms/bootc/-/merge_requests/98) | OK |
| c10s | RHEL-213825 | [!99](https://gitlab.com/redhat/centos-stream/rpms/bootc/-/merge_requests/99) | OK |

### JIRA links

- https://redhat.atlassian.net/browse/RHEL-213794
- https://redhat.atlassian.net/browse/RHEL-213807
- https://redhat.atlassian.net/browse/RHEL-213808
- https://redhat.atlassian.net/browse/RHEL-213809
- https://redhat.atlassian.net/browse/RHEL-213824
- https://redhat.atlassian.net/browse/RHEL-213825
