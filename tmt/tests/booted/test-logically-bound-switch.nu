# number: 21
# tmt:
#   summary: Execute logically bound images tests for switching images
#   duration: 30m
#
# This test does:
# bootc image switch bootc-bound-image
# <verify bound images are pulled>
# <reboot>
# <verify booted state>
# bootc upgrade
# <verify new bound images are pulled>
# <reboot>
# <verify booted state>
#
# If CSTOR_DIST_AUTH is set (format: "username:password"), the test also includes
# an authenticated LBI from cstor-dist running on the host. This tests the auth
# fallback fix from https://github.com/bootc-dev/bootc/pull/1852

use std assert
use tap.nu

# This code runs on *each* boot.
bootc status
let st = bootc status --json | from json
let booted = $st.status.booted.image

# cstor-dist configuration for authenticated LBI testing
const CSTOR_DIST_IMAGE = "ghcr.io/cgwalters/cstor-dist:latest"
const CSTOR_DIST_USER = "testuser"
const CSTOR_DIST_PASS = "testpass"
const CSTOR_DIST_PORT = 8000

# Start cstor-dist with basic auth on localhost
# Returns the registry address (localhost:port)
def start_cstor_dist [] {
    # Check if CSTOR_DIST_AUTH is set to enable authenticated LBI testing
    if $env.CSTOR_DIST_AUTH? == null {
        return null
    }

    print "Starting cstor-dist with basic auth..."

    # Pull a test image that cstor-dist will serve
    # This image will be used as the authenticated LBI
    print "Pulling test image for cstor-dist to serve..."
    podman pull docker.io/library/alpine:latest

    # Run cstor-dist container with auth enabled
    # Mount the local containers storage so cstor-dist can serve images from it
    let storage_path = if ("/var/lib/containers/storage" | path exists) {
        "/var/lib/containers/storage"
    } else {
        $"($env.HOME)/.local/share/containers/storage"
    }

    (podman run --privileged --rm -d --name cstor-dist-auth
        -p $"($CSTOR_DIST_PORT):8000"
        -v $"($storage_path):/var/lib/containers/storage"
        $CSTOR_DIST_IMAGE --username $CSTOR_DIST_USER --password $CSTOR_DIST_PASS)

    # Wait for cstor-dist to be ready
    print "Waiting for cstor-dist to be ready..."
    sleep 2sec

    # Verify it's running
    let running = podman ps --filter name=cstor-dist-auth --format "{{.Names}}" | str trim
    if $running != "cstor-dist-auth" {
        print "WARNING: cstor-dist container not running, skipping auth test"
        return null
    }

    print $"cstor-dist running on localhost:($CSTOR_DIST_PORT)"
    $"localhost:($CSTOR_DIST_PORT)"
}

# Get cstor-dist config if it's running
def get_cstor_dist_config [] {
    if $env.CSTOR_DIST_AUTH? == null {
        return null
    }

    let registry = $"localhost:($CSTOR_DIST_PORT)"
    # Base64 encode the credentials for auth.json
    let auth_b64 = $"($CSTOR_DIST_USER):($CSTOR_DIST_PASS)" | encode base64
    {
        registry: $registry,
        auth_b64: $auth_b64,
        # Use alpine image that we pulled and cstor-dist is serving
        image: "docker.io/library/alpine:latest"
    }
}

# Set up auth.json with cstor-dist credentials if available
def setup_auth [] {
    mkdir /run/ostree
    let cstor = get_cstor_dist_config
    if $cstor != null {
        print $"Setting up auth for cstor-dist at ($cstor.registry)"
        let auth_json = $'{"auths": {"($cstor.registry)": {"auth": "($cstor.auth_b64)"}}}'
        echo $auth_json | save -f /run/ostree/auth.json
        # Configure insecure registry for cstor-dist (no TLS)
        mkdir /etc/containers/registries.conf.d
        echo $"[[registry]]\nlocation=\"($cstor.registry)\"\ninsecure=true" | save -f /etc/containers/registries.conf.d/99-cstor-dist.conf
    } else {
        # The tests here aren't fetching from a registry which requires auth by default,
        # but we can replicate the failure in https://github.com/bootc-dev/bootc/pull/1852
        # by just injecting any auth file.
        echo '{}' | save -f /run/ostree/auth.json
    }
}

setup_auth

def initial_setup [] {
    bootc image copy-to-storage
    podman images
    podman image inspect localhost/bootc | from json
}

def build_image [name images containers] {
    let td = mktemp -d
    cd $td
    mkdir usr/share/containers/systemd

    mut dockerfile = "FROM localhost/bootc
COPY usr/ /usr/
RUN echo sanity check > /usr/share/bound-image-sanity-check.txt
" | save Dockerfile

    for image in $images {
        echo $"[Image]\nImage=($image.image)" | save $"usr/share/containers/systemd/($image.name).image"
        if $image.bound == true {
            # these extra RUNs are suboptimal
            # however, this is just a test image and the extra RUNs will only add a couple extra layers
            # the benefit is simplified file creation, i.e. we don't need to handle adding "&& \" to each line
            echo $"RUN ln -s /usr/share/containers/systemd/($image.name).image /usr/lib/bootc/bound-images.d/($image.name).image\n" | save Dockerfile --append
        }
    }

    for container in $containers {
        echo $"[Container]\nImage=($container.image)" | save $"usr/share/containers/systemd/($container.name).container"
        if $container.bound == true {
            echo $"RUN ln -s /usr/share/containers/systemd/($container.name).container /usr/lib/bootc/bound-images.d/($container.name).container\n" | save Dockerfile --append
        }
    }

    # Build it
    podman build -t $name .
    # Just sanity check it
    let v = podman run --rm $name cat /usr/share/bound-image-sanity-check.txt | str trim
    assert equal $v "sanity check"
}

def verify_images [images containers] {
    let bound_images = $images | where bound == true
    let bound_containers = $containers | where bound == true
    let num_bound = ($bound_images | length) + ($bound_containers | length)

    let image_names = podman --storage-opt=additionalimagestore=/usr/lib/bootc/storage images --format json | from json | select -i Names

    for $image in $bound_images {
        let found = $image_names | where Names == [$image.image]
        assert (($found | length) > 0) $"($image.image) not found"
    }

    for $container in $bound_containers {
        let found = $image_names | where Names == [$container.image]
        assert (($found | length) > 0) $"($container.image) not found"
    }
}

def first_boot [] {
    tap begin "bootc switch with bound images"

    initial_setup

    # Start cstor-dist if auth testing is enabled
    start_cstor_dist

    # build a bootc image that includes bound images
    mut images = [
        { "bound": true, "image": "registry.access.redhat.com/ubi9/ubi-minimal:9.4", "name": "ubi-minimal" },
        { "bound": false, "image": "quay.io/centos-bootc/centos-bootc:stream9", "name": "centos-bootc" }
    ]

    # Add authenticated LBI from cstor-dist if configured
    let cstor = get_cstor_dist_config
    if $cstor != null {
        print $"Adding authenticated LBI from cstor-dist: ($cstor.registry)/($cstor.image)"
        $images = ($images | append [{
            "bound": true,
            "image": $"($cstor.registry)/($cstor.image)",
            "name": "cstor-auth-test"
        }])
    }

    let containers = [{
        "bound": true, "image": "docker.io/library/alpine:latest", "name": "alpine"
    }]

    let image_name = "localhost/bootc-bound"
    build_image $image_name $images $containers
    bootc switch --transport containers-storage $image_name
    verify_images $images $containers
    tmt-reboot
}

def second_boot [] {
    print "verifying second boot after switch"
    assert equal $booted.image.transport containers-storage
    assert equal $booted.image.image localhost/bootc-bound

    # Start cstor-dist again if auth testing is enabled (container doesn't survive reboot)
    start_cstor_dist

    # verify images are still there after boot
    mut images = [
        { "bound": true, "image": "registry.access.redhat.com/ubi9/ubi-minimal:9.4", "name": "ubi-minimal" },
        { "bound": false, "image": "quay.io/centos-bootc/centos-bootc:stream9", "name": "centos-bootc" }
    ]

    # Add authenticated LBI from cstor-dist if configured
    let cstor = get_cstor_dist_config
    if $cstor != null {
        $images = ($images | append [{
            "bound": true,
            "image": $"($cstor.registry)/($cstor.image)",
            "name": "cstor-auth-test"
        }])
    }

    let containers = [{
        "bound": true, "image": "docker.io/library/alpine:latest", "name": "alpine"
    }]
    verify_images $images $containers

    # build a new bootc image with an additional bound image
    print "bootc upgrade with another bound image"
    let image_name = "localhost/bootc-bound"
    let more_images = $images | append [{ "bound": true, "image": "registry.access.redhat.com/ubi9/ubi-minimal:9.3", "name": "ubi-minimal-9-3" }]
    build_image $image_name $more_images $containers
    bootc upgrade
    verify_images $more_images $containers
    tmt-reboot
}

def third_boot [] {
    print "verifying third boot after upgrade"
    assert equal $booted.image.transport containers-storage
    assert equal $booted.image.image localhost/bootc-bound

    mut images = [
        { "bound": true, "image": "registry.access.redhat.com/ubi9/ubi-minimal:9.4", "name": "ubi-minimal" },
        { "bound": true, "image": "registry.access.redhat.com/ubi9/ubi-minimal:9.3", "name": "ubi-minimal-9-3" },
        { "bound": false, "image": "quay.io/centos-bootc/centos-bootc:stream9", "name": "centos-bootc" }
    ]

    # Add authenticated LBI from cstor-dist if configured
    let cstor = get_cstor_dist_config
    if $cstor != null {
        $images = ($images | append [{
            "bound": true,
            "image": $"($cstor.registry)/($cstor.image)",
            "name": "cstor-auth-test"
        }])
    }

    let containers = [{
        "bound": true, "image": "docker.io/library/alpine:latest", "name": "alpine"
    }]

    verify_images $images $containers
    tap ok
}

def main [] {
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        "2" => third_boot,
        $o => { error make { msg: $"Invalid TMT_REBOOT_COUNT ($o)" } },
    }
}
