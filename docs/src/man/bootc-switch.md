# NAME

bootc-switch - Target a new container image reference to boot

# SYNOPSIS

**bootc switch** \[**\--quiet**\] \[**\--apply**\] \[**\--transport**\]
\[**\--enforce-container-sigpolicy**\] \[**\--retain**\]
\[**\--download-only**\|**\--from-downloaded**\]
\[**-h**\|**\--help**\] \[\<*TARGET*\>\]

# DESCRIPTION

Target a new container image reference to boot.

This is almost exactly the same operation as \`upgrade\`, but
additionally changes the container image reference instead.

\## Usage

A common pattern is to have a management agent control operating system
updates via container image tags; for example,
\`quay.io/exampleos/someuser:v1.0\` and
\`quay.io/exampleos/someuser:v1.1\` where some machines are tracking
\`:v1.0\`, and as a rollout progresses, machines can be switched to
\`v:1.1\`.

# OPTIONS

**\--quiet**

:   Dont display progress

**\--apply**

:   Restart or reboot into the new target image.

    Currently, this option always reboots. In the future this command
    will detect the case where no kernel changes are queued, and perform
    a userspace-only restart.

**\--transport**=*TRANSPORT* \[default: registry\]

:   The transport; e.g. oci, oci-archive, containers-storage. Defaults
    to \`registry\`

**\--enforce-container-sigpolicy**

:   This is the inverse of the previous
    \`\--target-no-signature-verification\` (which is now a no-op).

    Enabling this option enforces that \`/etc/containers/policy.json\`
    includes a default policy which requires signatures.

**\--retain**

:   Retain reference to currently booted image

**\--download-only**

:   Download and stage the update without applying it.

**\--from-downloaded**

:   Apply a staged deployment previously downloaded with **\--download-only**.

**-h**, **\--help**

:   Print help (see a summary with -h)

\<*TARGET*\>

:   Target image to use for the next boot

# VERSION

v1.1.4
