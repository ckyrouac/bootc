---
nav_order: 7
---

# Quickstart

This is a reference for developers interested in contributing to bootc. This is intended to be a getting started guide to help facilitate setting up a development environment to test changes.

## Setup a bootc VM

The following are instructions to quickly setup a VM with a bootc image based operating system. The goal of this quickstart is to enable you to start exporing a basic bootc environment and to provide a place to test your changes to bootc.

1. Create a bootc image Containerfile. More complex Containerfiles are in the [centos-bootc-layered repo](https://github.com/CentOS/centos-bootc-layered/tree/main/examples).

```bash
mkdir bootc-quickstart && cd bootc-quickstart
```

3. Create a disk image for bootc to install the container into

```bash
truncate -s 10G bootc-quickstart.raw
```

4. Install the container into the image using bootc

```bash
sudo podman run --pid=host --network=host --privileged --security-opt label=type:unconfined_t -v /var/lib/containers:/var/lib/containers -v .:/bootc-quickstart centos-bootc bootc install to-disk --generic-image --via-loopback --skip-fetch-check ./bootc-quickstart/bootc-quickstart.raw
```

5. Start a VM using the new disk image. The following command is for libvirt in Fedora.

```bash
virt-install --memory=1024 --vcpus=1 --import --disk=bootc-quickstart.raw --name bootc-quickstart --os-variant=rhel9.4
```

## Debugging

Using lldb, it is possible to run the bootc executable in a container while debugging the code locally. This is useful for some of the bootc cli commands that don't require an existing bootc filesystem.

1. Create a Containerfile with lldb installed

```dockerfile
FROM quay.io/centos-bootc/centos-bootc:stream9
COPY bootc /usr/bin/bootc
RUN mkdir -p /var/lib/alternatives && \
    rm -r /opt && \
    dnf -y install lldb
```

2. Build the lldb image

```bash
podman build . --tag centos-bootc-lldb
```

3. Start the lldb server in a container

```bash
sudo podman run --pid=host --network=host --privileged --security-opt label=type:unconfined_t -v /var/lib/containers:/var/lib/containers -v .:/bootc-quickstart centos-bootc-lldb lldb-server platform --listen "*:1234" --server
```

4. Connect to the lldb server from an lldb client. This will depend on your IDE. This is how to connect via the lldb cli:

```bash

```
