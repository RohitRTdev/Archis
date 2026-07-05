# ARCHIS
ARCHIS is targeted to be a cross-platform multi-arch supporting Rust based OS.

## Description
Current plan is to support x86_64 architecture for intel/amd chipsets which translates to UEFI platform for most modern machines.

## Prerequisites
The build has been tested on Windows, WSL and Ubuntu, and for now, it works<br>
Tools required
* Rust (rustc-nightly == 1.98.0)
* Docker (only required for windows)
* Make
* Clang
* llvm-ar
* Standard GNU tools

If you're on windows, simply download MSYS2, and use pacman (MSYS2 default package manager) to download the required tools. 
Make sure to add the usr/bin folder to system path (MSYS2 by default doesn't add that to system path)

## Build
To build the OS, run following from project root
>make

This will build debug version of kernel, bootloader, drivers and the disk image<br>
If you want release version, simply invoke
>make CONFIG=release

## Status
The current state of project
* Starts with a simple shell that can execute processes. Understands foreground process groups, signals, pipe, redirection etc
* Userspace utilities cannot use floating point operations right now, the scheduler doesn't save floating point state as part of the thread context yet.

## Todo
* Implement virtio blk driver. If need be later look at usb
* Write basic userspace utilities

## Testing 
Testing can be done by burning the image file to a flash drive (tools like rufus or balena etcher should be fine) and running it on real machine by choosing to boot through the flash drive in BIOS setup.
Make sure that you have disabled secure boot in the BIOS setup or the OS won't load.

The image file can also be loaded in qemu and run.
Download qemu for your platform and simply invoke
>make test

We also have provision to run unit tests, this is primarily for development purposes.
>make run_unit_test

These tests are run on host OS, which means they are designed to only test logical functionality (Like allocator/loader working) and not meant for hardware testing,
which still requires simulation


