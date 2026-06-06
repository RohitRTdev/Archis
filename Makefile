CONFIG ?= debug
BUILD_CONFIG ?= x86_64-acpi-uefi
CONFIG_FILE := config/$(BUILD_CONFIG).mk
IMAGE_NAME = disk-tools
OUTPUT_DIR = output
ENV_PLACEHOLDER = placeholder.txt
KERN_PLACEHOLDER = kernel/placeholder_test.txt
GEN_MSG = "Automatically generated file..\nDo not remove manually.."
SHELL = /bin/bash

ifeq ($(wildcard $(CONFIG_FILE)),)
$(error Config file '$(CONFIG_FILE)' not found!)
endif

include $(CONFIG_FILE)

KERNEL_FLAGS := -C link-arg=-T$(LINKER_SCRIPT)
DRIVER_FLAGS := -C link-arg=-T$(DRIVER_LINKER_SCRIPT) -C link-arg=-Ltarget/$(KERNEL_ARCH)/$(CONFIG)

ifeq ($(CONFIG),release)
    BUILD_OPTIONS := --release
else ifeq ($(CONFIG),debug)
    BUILD_OPTIONS :=
else 
$(error Config flag must be either 'debug' or 'release')
endif

ifeq ($(CONFIG),debug)
    KERNEL_FLAGS += -C force-frame-pointers=yes
    DRIVER_FLAGS += -C force-frame-pointers=yes
endif

.PHONY: all clean build_blr build_kernel build_kernel_test build_kernel_template build_image

all: build_image

$(ENV_PLACEHOLDER): 
	@echo "Setting up virtual env for image creation"
	@docker build -t $(IMAGE_NAME) ./scripts
	@echo -e $(GEN_MSG) > $(ENV_PLACEHOLDER)

build_image: build_kernel build_blr build_drivers $(ENV_PLACEHOLDER)
	@echo "Starting image creation"
	@touch $(OUTPUT_IMAGE)
	$(RUN_DOCKER_SCRIPT)

$(OUTPUT_DIR):
	@mkdir -p $(OUTPUT_DIR)

build_blr: $(OUTPUT_DIR)
	@rustup target list --installed | grep -qx $(BLR_TARGET) || {\
		echo "Adding blr target configuration";\
		rustup target add $(BLR_TARGET);\
	}
	@echo "Building bootloader..." 
	@(cd $(BLR_CRATE_PATH) && \
		cargo build $(BUILD_OPTIONS) $(BLR_OPTIONS) \
		-Z build-std=core,alloc \
		--target $(BLR_TARGET) \
	)
	@cp $(BLR_EXE) $(BLR_TARGET_EXE) 

build_kernel_template:
	@echo "Building kernel..."
	@(cd kernel && RUSTFLAGS="$(KERNEL_FLAGS)" \
		cargo build $(BUILD_OPTIONS) $(KERNEL_OPTIONS) \
		-Z build-std=core,compiler_builtins,alloc \
		-Z build-std-features=compiler-builtins-mem \
		--target $(KERNEL_TARGET) \
	)
	@cp $(KERNEL_EXE) $(OUTPUT_DIR)/aris
	@cp config/initfs.conf $(OUTPUT_DIR)

build_kernel: $(OUTPUT_DIR)
	@if [ -f "$(KERN_PLACEHOLDER)" ]; then cargo clean; fi
	@rm -f $(KERN_PLACEHOLDER)	
	@make build_kernel_template

build_kernel_test: $(OUTPUT_DIR)
	@if [ ! -f "$(KERN_PLACEHOLDER)" ]; then cargo clean; fi	
	@echo -e $(GEN_MSG) > $(KERN_PLACEHOLDER) 
	@make build_kernel_template

build_drivers: build_kernel
	@echo "Building drivers..."
	@mkdir -p $(OUTPUT_DIR)/drivers
	@set -e; for name in $$(cat target/driver_deps.txt); do \
		driver_path="kernel/src/drivers/$$name"; \
		if [ -f $$driver_path/Cargo.toml ]; then \
			echo "Building driver $$name"; \
			(cd $$driver_path && \
				RUSTFLAGS="$(DRIVER_FLAGS)" \
				cargo build $(BUILD_OPTIONS) \
				-Z build-std=core,compiler_builtins,alloc \
				-Z build-std-features=compiler-builtins-mem \
				--target ../../../$(KERNEL_TARGET)); \
			cp target/$(KERNEL_ARCH)/$(CONFIG)/lib$$name.so $(OUTPUT_DIR)/drivers; \
		fi \
	done
	@cp kernel/src/drivers/boot.conf $(OUTPUT_DIR)/drivers/ || echo "boot.conf not found..."

run_unit_test: build_kernel_test
	@cargo test --manifest-path=boot/blr/Cargo.toml -- --nocapture
	@cargo test --features test-kernel --manifest-path=kernel/Cargo.toml -- --nocapture

test:
	@echo "Starting simulator..."
	@qemu-system-x86_64 -smp sockets=1,cores=6,threads=2 -cpu Skylake-Client,+smap,+smep,+umip,+pge -bios scripts/OVMF.fd\
 -drive file=$(OUTPUT_DIR)/archis_os.iso,format=raw,if=ide -serial mon:stdio | tee >(sed 's/\x1b\[[0-9;=]*[A-Za-z]//g' > $(OUTPUT_DIR)/con_log.txt)

clean:
	@echo "Cleaning all builds..."
	@cargo clean
	@rm -rf $(OUTPUT_DIR)

# Execute this to restart build process from very beginning
# Use this if facing some problems with build
reset: clean
	@echo "Removing placeholders"
	@rm -f $(KERN_PLACEHOLDER) $(ENV_PLACEHOLDER)
