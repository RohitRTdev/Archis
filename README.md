# ARCHIS
ARCHIS is targeted to be a cross-platform multi-arch supporting Rust based OS.

## Description
Current plan is to support x86_64 architecture for intel/amd chipsets which translates to UEFI platform for most modern machines.

## Prerequisites
The build has been tested on Windows, WSL and Ubuntu, and for now, it works<br>
Tools required
* Rust (rustc-nightly >= 1.94.0)
* Docker
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
* UEFI bootloader is setup, which allows the OS to run in modern UEFI based platforms 
* Hal for x86_64 and platform initialization is complete. 
* Interrupt subsystem is ready. This allows any thread to now install/uninstall interrupt handlers for any IRQ.
* Kernel can now create/destroy processes/threads, have separate virtual address space for each.
* Ability to allocate/deallocate, map/unmap virtual memory. The kernel half of memory is shared among all processes though. 
* Load kernel modules dynamically (all drivers are planned to be currently loaded as dynamic modules instead of statically linked rust libraries). Each module may depend on other modules, in which case the kernel loads all dependencies first and patches the .plt sections of the ELF to ensure that the modules can call each other's functions
* Process manager also has ability to load a user thread/process and kernel has established syscall handler which means that user mode processes can now talk to the kernel. 
* Processes/threads can wait on semaphores with/without timeout. Threads can signal each other. The blocked thread is put into sleep (not just busy waiting) and woken up accordingly. If thread which was waiting on timer or semaphore is killed (by another thread), then this thread is removed from the semaphore blocked list.
* In ACPI-UEFI based x86_64 platform, Aris uses RTC to read the wall clock time and timestamps are read from the cpu's tsc counter (both these can be seen in every log that is printed by any kernel module). The delay_ns function uses the platform HPET timer (We use this because the frequency for it is configurable). The delay_ms uses the timer interrupt to note the elapsed time. 
The timer interrupt is fired using the CPU's LAPIC timer. Also, the lapic timer's frequency is measured using HPET (see [hal/x86_64/timer.rs](kernel/src/hal/x86_64/timer.rs) init function). This also means that if the platform doesn't have HPET timer, then currently the kernel panics as we don't have another reliable way to measure the CPU LAPIC frequency.

## Todo
* Implement shared memory, pipes
* Implement acpi, pci driver
* Start with virtio blk driver. If need be later look at usb
* Fat32 file system driver and mount logic
* Implement file ops in libc
* Write init and shell process
* Write basic userspace utilities
* Logic to wait_for_multiple_semaphores

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


