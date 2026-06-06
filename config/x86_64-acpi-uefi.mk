BLR_TARGET = x86_64-unknown-uefi
KERNEL_ARCH = x86_64
KERNEL_TARGET = config/x86_64/x86_64.json
BLR_CRATE_PATH = boot/uefi
KERNEL_CRATE_PATH = kernel
BLR_EXE = target/x86_64-unknown-uefi/$(CONFIG)/boot.efi
BLR_TARGET_EXE = $(OUTPUT_DIR)/bootx64.efi 
KERNEL_EXE = target/x86_64/$(CONFIG)/libaris.so
LINKER_SCRIPT = kernel/config/x86_64/linker.ld
DRIVER_LINKER_SCRIPT = kernel/config/x86_64/driver_linker.ld
OUTPUT_IMAGE = $(OUTPUT_DIR)/archis_os.iso
PLATFORM = acpi
BOOTLOADER = uefi
KERNEL_OPTIONS = --features acpi,stack_down,debug-scheduler-logs,debug-mem-logs
BLR_OPTIONS = --features acpi

ifeq ($(OS),Windows_NT)
    RUN_DOCKER_SCRIPT = @./scripts/docker.bat
else
    RUN_DOCKER_SCRIPT = @docker run -it --privileged -v "$$(pwd)":/workspace -w /workspace $(IMAGE_NAME) ./scripts/create_image_uefi.sh
endif
