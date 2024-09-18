#!/bin/bash

set -e

# build the container image
podman build --build-arg "sshpubkey=$(cat ~/.ssh/id_rsa.pub)" -f Containerfile.bound -t quay.io/ckyrouac/bootc-lldb-bound:latest .
podman push quay.io/ckyrouac/bootc-lldb-bound:latest
