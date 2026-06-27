CONFIG ?= debug
BUILD_CONFIG ?= x86_64/x86_64-acpi-uefi
CONFIG_FILE := config/$(BUILD_CONFIG).mk
IMAGE_NAME := disk-tools
OUTPUT_DIR := output
ENV_PLACEHOLDER := placeholder.txt
KERN_PLACEHOLDER := kernel/placeholder_test.txt
QEMU_CPU_ARGS_WITH_ACCEL := -M q35,accel=kvm -cpu host -smp sockets=1,cores=2,threads=6
QEMU_CPU_ARGS_WITHOUT_ACCEL := -smp sockets=1,cores=2,threads=6 -cpu Skylake-Client,+smap,+smep,+umip,+pge
GEN_MSG := "Automatically generated file..\nDo not remove manually.."
USERSPACE_FLAGS = ARCH=$(KERNEL_ARCH) ARCH_TARGET=$(KERNEL_TARGET_TRIPLE) USER_LINKER_SCRIPT=../$(USER_LINKER_SCRIPT) OBJDIR=../target/userspace OUTDIR=../$(OUTPUT_DIR)/bin 
SHELL := /bin/bash

ifeq ($(wildcard $(CONFIG_FILE)),)
$(error Config file '$(CONFIG_FILE)' not found!)
endif

include $(CONFIG_FILE)

KERNEL_FLAGS := -C link-arg=-T$(LINKER_SCRIPT)
MODULE_FLAGS := -C link-arg=-T$(MODULE_LINKER_SCRIPT) -C link-arg=-Ltarget/$(KERNEL_ARCH)/$(CONFIG)

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

.PHONY: all clean build_blr build_kernel build_kernel_test build_kernel_template build_image build_userspace

all: build_image

$(ENV_PLACEHOLDER):
ifeq ($(NO_DOCKER),true)
	@echo "Skipping docker build on linux system..."
else 
	@echo "Setting up virtual env for image creation" 
	@docker build -t $(IMAGE_NAME) ./scripts 
endif
	@echo -e $(GEN_MSG) > $(ENV_PLACEHOLDER)

build_image: build_kernel build_blr build_drivers build_modules build_userspace $(ENV_PLACEHOLDER)
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

build_kernel_template: config/initfs.conf
	@echo "Building kernel..."
	@(cd kernel && RUSTFLAGS="$(KERNEL_FLAGS)" \
		cargo build $(BUILD_OPTIONS) $(KERNEL_OPTIONS) \
		-Z json-target-spec \
		-Z build-std=core,compiler_builtins,alloc \
		-Z build-std-features=compiler-builtins-mem \
		--target ../$(KERNEL_TARGET) \
	)
	@cp $(KERNEL_EXE) $(OUTPUT_DIR)/aris
	@cp config/initfs.conf $(OUTPUT_DIR)

build_kernel: $(OUTPUT_DIR)
	@mkdir -p target
	@printf "%s\n" $(DRIVER_LIST) > target/input_driver_list.txt
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
				RUSTFLAGS="$(MODULE_FLAGS)" \
				cargo build $(BUILD_OPTIONS) \
				-Z json-target-spec \
				-Z build-std=core,compiler_builtins,alloc \
				-Z build-std-features=compiler-builtins-mem \
				--target ../../../../$(KERNEL_TARGET)); \
			cp target/$(KERNEL_ARCH)/$(CONFIG)/lib$$name.so $(OUTPUT_DIR)/drivers; \
		fi \
	done
	@cp kernel/src/drivers/boot.conf $(OUTPUT_DIR)/drivers/ || echo "boot.conf not found..."

build_modules: build_kernel
	@echo "Building kernel modules..."
	@set -e; for module_path in modules/*; do \
		if [ -d "$$module_path" ] && [ -f "$$module_path/Cargo.toml" ]; then \
			name=$$(basename "$$module_path"); \
			echo "Building module $$name"; \
			(cd "$$module_path" && \
				RUSTFLAGS="$(MODULE_FLAGS)" \
				cargo build $(BUILD_OPTIONS) \
				-Z json-target-spec \
				-Z build-std=core,compiler_builtins,alloc \
				-Z build-std-features=compiler-builtins-mem \
				--target ../../$(KERNEL_TARGET)); \
			cp target/$(KERNEL_ARCH)/$(CONFIG)/lib$$name.so $(OUTPUT_DIR); \
		fi; \
	done

build_userspace: $(OUTPUT_DIR)
	@echo "Building userspace programs..."
	@mkdir -p target/userspace
	@cd userspace && $(MAKE) $(USERSPACE_FLAGS) all

run_unit_test: build_kernel_test
	@cargo test --manifest-path=boot/blr/Cargo.toml -- --nocapture
	@cargo test --features test-kernel --manifest-path=kernel/Cargo.toml -- --nocapture

test:
	@echo "Starting simulator..."
	@qemu-system-x86_64 $(QEMU_CPU_ARGS_WITH_ACCEL) \
	-drive if=pflash,format=raw,readonly=on,file=scripts/OVMF.fd \
	-drive file=$(OUTPUT_DIR)/archis_os.img,format=raw,if=ide -m 512M -serial stdio | tee >(sed 's/\x1b\[[0-9;=]*[A-Za-z]//g' > $(OUTPUT_DIR)/con_log.txt)

clean:
	@echo "Cleaning all builds..."
	@cd userspace && $(MAKE) clean $(USERSPACE_FLAGS)
	@cargo clean
	@rm -rf $(OUTPUT_DIR)

# Execute this to restart build process from very beginning
# Use this if facing some problems with build
reset: clean
	@echo "Removing placeholders"
	@rm -f $(KERN_PLACEHOLDER) $(ENV_PLACEHOLDER)
