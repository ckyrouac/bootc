#!/bin/bash
set -xeuo pipefail

target_image=localhost/bootc-to-existing-root-preserve-var-merge-etc:latest
archive_name=bootc-to-existing-root-preserve-var-merge-etc.oci
archive_dir="${TMT_PLAN_DATA:-}/package-mode-transfer"
target_archive="$archive_dir/$archive_name"
target_archive_checksum="$target_archive.sha256"
var_marker=/var/lib/bootc-tmt-preserve-var/sentinel
etc_dir=/etc/bootc-tmt-merge
package_marker=/root/bootc-tmt-package-mode-marker
package_kernel=/root/bootc-tmt-package-kernel
package_entry=/root/bootc-tmt-package-mode-entry
package_entry_copy=/root/bootc-tmt-package-mode-entry.conf

case ${TMT_REBOOT_COUNT:-0} in
    0)
        # TMT pushes plan data to the selected test guest before execute. Fail
        # clearly if either staged file did not arrive with that data.
        if [[ -z "${TMT_PLAN_DATA:-}" ]]; then
            echo 'TMT_PLAN_DATA is missing in the package-mode test guest' >&2
            exit 1
        fi
        if [[ ! -f "$target_archive" || -L "$target_archive" || ! -s "$target_archive" ]]; then
            echo "Package-mode image archive is missing or invalid: $target_archive" >&2
            exit 1
        fi
        if [[ ! -f "$target_archive_checksum" || -L "$target_archive_checksum" ]]; then
            echo "Package-mode image archive checksum is missing or invalid: $target_archive_checksum" >&2
            exit 1
        fi
        (cd "$archive_dir" && sha256sum -c -- "$archive_name.sha256")
        podman --remote=false load < "$target_archive"
        podman --remote=false image exists "$target_image"
        rm -- "$target_archive" "$target_archive_checksum"

        # The target guest must still be the original package-mode system.
        test ! -e /run/ostree-booted

        # Exercise both an admin-created file and an admin modification of an
        # image default. The derived image below supplies the pristine values.
        mkdir -p "$etc_dir" "${var_marker%/*}"
        printf '%s\n' package-mode > "$package_marker"
        uname -r > "$package_kernel"
        mapfile -t running_entries < <(
            for entry in /boot/loader/entries/*.conf; do
                test "$(sed -n 's/^version //p' "$entry")" = "$(uname -r)" || continue
                grep -q '^linux ' "$entry" || continue
                grep -q '^initrd ' "$entry" || continue
                grep -q '^options ' "$entry" || continue
                grep -q '^options .*ostree=' "$entry" && continue
                printf '%s\n' "$entry"
            done
        )
        test "${#running_entries[@]}" -eq 1
        running_entry=${running_entries[0]}
        basename "$running_entry" > "$package_entry"
        cp "$running_entry" "$package_entry_copy"

        # TMT connects as root. Package-mode hosts keep root's home at /root,
        # while bootc images link /root to /var/roothome, so preserve its key
        # under /var for TMT to reconnect after the migration reboot.
        install -d -m 0700 /var/roothome/.ssh
        install -m 0600 /root/.ssh/authorized_keys /var/roothome/.ssh/authorized_keys
        restorecon -RF /var/roothome/.ssh

        printf '%s\n' admin-modified | tee "$etc_dir/default.conf"
        printf '%s\n' admin-only | tee "$etc_dir/admin-only.conf"
        printf '%s\n' preserved-var | tee "$var_marker"

        # The flags require the OSTree backend. Run the current build-under-test
        # as a privileged installer, with the package-mode root mounted at /target.
        podman --remote=false run --rm --privileged --pid=host --user=root:root \
            -v /dev:/dev \
            -v /:/target \
            -v /var/lib/containers:/var/lib/containers \
            --security-opt label=type:unconfined_t \
            "$target_image" \
            bootc install to-existing-root \
                --preserve-var \
                --preserve-package-boot \
                --merge-etc \
                --acknowledge-destructive

        bootc status
        # Ask the guest to reboot itself; this is more reliable than
        # testcloud's ACPI soft-poweroff for a full bootc deployment reboot.
        tmt-reboot -c 'systemctl reboot'
        ;;
    1)
        test -e /run/ostree-booted

        booted_image=$(bootc status --json | jq -r '.status.booted.image.image.image')
        test "$booted_image" = "$target_image"

        test "$(<"$var_marker")" = preserved-var
        test "$(<"$etc_dir/default.conf")" = admin-modified
        test "$(<"$etc_dir/admin-only.conf")" = admin-only
        test "$(<"$etc_dir/image-only.conf")" = image-only

        # The original package-mode entry and all of its assets must remain
        # selectable after bootupd installs the new GRUB BLS loader.
        rollback_entry="/boot/loader/entries/$(<"/sysroot$package_entry")"
        test -f "$rollback_entry"
        cmp "$rollback_entry" "/sysroot$package_entry_copy"
        rollback_title=$(sed -n 's/^title //p' "$rollback_entry")
        test -n "$rollback_title"
        test "$(sed -n 's/^version //p' "$rollback_entry")" = "$(<"/sysroot$package_kernel")"
        while read -r _ assets; do
            for asset in $assets; do
                test -f "/boot/${asset#/}"
            done
        done < <(grep -E '^(linux|initrd|devicetree) ' "$rollback_entry")
        ! grep -q '^options .*ostree=' "$rollback_entry"
        test "$(<"/sysroot$package_marker")" = package-mode

        grub2-reboot "$rollback_title"
        tmt-reboot -c 'systemctl reboot'
        ;;
    2)
        # Actually boot the previous package-mode OS, not merely retain a BLS
        # file. This checks both the package kernel/initrd and its old root.
        test ! -e /run/ostree-booted
        test "$(<"$package_marker")" = package-mode
        test "$(uname -r)" = "$(<"$package_kernel")"
        ;;
    *)
        printf 'Unexpected TMT_REBOOT_COUNT=%s\n' "$TMT_REBOOT_COUNT" >&2
        exit 1
        ;;
esac
