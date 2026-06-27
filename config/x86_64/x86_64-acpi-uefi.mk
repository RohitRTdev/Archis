BLR_TARGET := x86_64-unknown-uefi
KERNEL_ARCH := x86_64
KERNEL_TARGET := config/x86_64/x86_64.json
KERNEL_TARGET_TRIPLE := x86_64-unknown-none
BLR_CRATE_PATH := boot/uefi
KERNEL_CRATE_PATH := kernel
BLR_EXE := target/x86_64-unknown-uefi/$(CONFIG)/boot.efi
BLR_TARGET_EXE = $(OUTPUT_DIR)/bootx64.efi 
KERNEL_EXE := target/x86_64/$(CONFIG)/libaris.so
LINKER_SCRIPT := kernel/config/x86_64/linker.ld
MODULE_LINKER_SCRIPT := config/x86_64/module_linker.ld
USER_LINKER_SCRIPT := config/x86_64/user_linker.ld
OUTPUT_IMAGE = $(OUTPUT_DIR)/archis_os.img
PLATFORM := acpi
BOOTLOADER := uefi
KERNEL_OPTIONS := --features acpi,stack_down,debug-loader-logs
BLR_OPTIONS := --features acpi
NO_DOCKER := false

ifeq ($(OS),Windows_NT)
    RUN_DOCKER_SCRIPT := @./scripts/docker.bat
else
    RUN_DOCKER_SCRIPT := @sudo ./scripts/create_image_uefi.sh
    NO_DOCKER := true
endif
