ifndef ARCH_TARGET
$(error ARCH_TARGET must be supplied)
endif

ifndef USER_LINKER_SCRIPT
$(error USER_LINKER_SCRIPT must be supplied)
endif

ifndef OBJDIR
$(error OBJDIR must be supplied)
endif

ifndef OUTDIR
$(error OUTDIR must be supplied)
endif

CC := clang
AR := llvm-ar

TARGET := $(ARCH_TARGET)

RUST_HOST := $(shell rustc -vV | grep '^host:' | cut -d' ' -f2)
RUST_LLD := $(shell rustc --print sysroot)/lib/rustlib/$(RUST_HOST)/bin/rust-lld

LINKER_SCRIPT := $(USER_LINKER_SCRIPT)
USER_LLD_FLAGS := -flavor gnu --hash-style=sysv -T $(LINKER_SCRIPT) -rpath /bin
COMMON_CFLAGS := -target $(TARGET) -ffreestanding -fPIC -nostdinc -Wall -Wextra -MMD -MP -I$(CURDIR)/include

# crt object files will be compiled into every binary that is generated
CRT_SRCS := crt/*.c crt/$(ARCH)/*

# Mention the binaries to be compiled in this list
BINARIES := signal_test init sh producer producer2 producer3 consumer stdio_test envtest cat echo ls ln readlink mkdir rmdir rm shutdown fs_stress sleep dmesg ps kill

# Shared libraries (dynamically loaded) and static libraries(.a archives) are to be mentioned here
SHARED_LIBS := c
STATIC_LIBS :=

# Write down the src files and lib dependencies of each binary
# in following format -> <bin_name>_SRCS := <src_list>, <bin_name>_LIBS := <static_and_shared_lib_list>
signal_test_SRCS := signal_test.c
signal_test_LIBS := c

init_SRCS := init.c
init_LIBS := c

sh_SRCS := sh/*.c
sh_LIBS := c

producer_SRCS := producer.c
producer_LIBS := c

producer2_SRCS := producer2.c
producer2_LIBS := c

producer3_SRCS := producer3.c
producer3_LIBS := c

consumer_SRCS := consumer.c
consumer_LIBS := c

stdio_test_SRCS := stdio_test.c
stdio_test_LIBS := c

envtest_SRCS := envtest.c
envtest_LIBS := c

cat_SRCS := cat.c
cat_LIBS := c

echo_SRCS := echo.c
echo_LIBS := c

ls_SRCS := ls.c
ls_LIBS := c

ln_SRCS := ln.c
ln_LIBS := c

readlink_SRCS := readlink.c
readlink_LIBS := c

mkdir_SRCS := mkdir.c
mkdir_LIBS := c

rmdir_SRCS := rmdir.c
rmdir_LIBS := c

rm_SRCS := rm.c
rm_LIBS := c

shutdown_SRCS := shutdown.c
shutdown_LIBS := c

fs_stress_SRCS := fs_stress.c
fs_stress_LIBS := c

sleep_SRCS := sleep.c
sleep_LIBS := c

dmesg_SRCS := dmesg.c
dmesg_LIBS := c

ps_SRCS := ps.c
ps_LIBS := c

kill_SRCS := kill.c
kill_LIBS := c

c_SRCS := libc/*.c libc/*.s libc/$(ARCH)/*

expand-srcs = $(foreach p,$1,$(wildcard $(p)))
src-to-obj = $(OBJDIR)/$(patsubst %.c,%.o,$(patsubst %.S,%.o,$(patsubst %.s,%.o,$1)))
src-to-dep = $(OBJDIR)/$(patsubst %.c,%.d,$1)


# The makefile follows a design where all recipes are generated dynamically instead of 
# using a bash script to compile them in a for loop
# This allows us to use all existing features of makefile such as dependency resolution
# and compiling only what is needed

ALL_DEPS :=

define DEFINE_C_OBJECT

$(call src-to-obj,$1): $1
	@mkdir -p $$(@D)
	@echo Compiling $$<
	@$(CC) $(COMMON_CFLAGS) -c $$< -o $$@

ALL_DEPS += $(call src-to-dep,$1)

endef

define DEFINE_ASM_OBJECT

$(call src-to-obj,$1): $1
	@mkdir -p $$(@D)
	@echo Compiling $$<
	@$(CC) -target $(TARGET) -c $$< -o $$@

endef

ALL_SRCS :=

$(foreach x,$(BINARIES),$(eval ALL_SRCS += $(call expand-srcs,$($(x)_SRCS))))
$(foreach x,$(STATIC_LIBS),$(eval ALL_SRCS += $(call expand-srcs,$($(x)_SRCS))))
$(foreach x,$(SHARED_LIBS),$(eval ALL_SRCS += $(call expand-srcs,$($(x)_SRCS))))
$(foreach x,$(CRT_SRCS),$(eval ALL_SRCS += $(call expand-srcs,$(x))))

ALL_C_SRCS := $(sort $(filter %.c,$(ALL_SRCS)))
ALL_ASM_SRCS := $(sort $(filter %.s,$(ALL_SRCS)))

$(foreach s,$(ALL_C_SRCS),$(eval $(call DEFINE_C_OBJECT,$(s))))
$(foreach s,$(ALL_ASM_SRCS),$(eval $(call DEFINE_ASM_OBJECT,$(s))))

CRT_OBJECTS :=

$(foreach s,$(call expand-srcs,$(CRT_SRCS)),$(eval CRT_OBJECTS += $(call src-to-obj,$(s))))

ALL_TARGETS :=

define DEFINE_STATIC_LIBRARY

$(OBJDIR)/lib$1.a: $(foreach s,$(call expand-srcs,$($1_SRCS)),$(call src-to-obj,$(s)))
	@mkdir -p $$(@D)
	@echo Building static library $$(@F)
	@$(AR) rcs $$@ $$^

ALL_TARGETS += $(OBJDIR)/lib$1.a

endef

define DEFINE_SHARED_LIBRARY

$(OUTDIR)/lib$1.so: $(foreach s,$(call expand-srcs,$($1_SRCS)),$(call src-to-obj,$(s)))
	@mkdir -p $$(@D)
	@echo Building shared library $$(@F)
	@$(RUST_LLD) $(USER_LLD_FLAGS) --shared --soname=lib$1.so -o $$@  $$^

ALL_TARGETS += $(OUTDIR)/lib$1.so

endef

define STATIC_DEP_PATHS
$(foreach l,$($1_LIBS),$(if $(filter $(l),$(STATIC_LIBS)),$(OBJDIR)/lib$(l).a,))
endef

define SHARED_DEP_PATHS
$(foreach l,$($1_LIBS),$(if $(filter $(l),$(SHARED_LIBS)),$(OUTDIR)/lib$(l).so,))
endef

define STATIC_LINK_FLAGS
$(foreach l,$($1_LIBS),$(if $(filter $(l),$(STATIC_LIBS)),$(OBJDIR)/lib$(l).a,))
endef

define SHARED_LINK_FLAGS
$(foreach l,$($1_LIBS),$(if $(filter $(l),$(SHARED_LIBS)),-l$(l),))
endef

define DEFINE_BINARY

$(OUTDIR)/$1: \
$(CRT_OBJECTS) \
$(foreach s,$(call expand-srcs,$($1_SRCS)),$(call src-to-obj,$(s))) \
$(call STATIC_DEP_PATHS,$1) \
$(call SHARED_DEP_PATHS,$1)

	@mkdir -p $$(@D)
	@echo Linking binary $$(@F)
	@$(RUST_LLD) \
		$(USER_LLD_FLAGS) -pie \
		-o $$@ \
		$(CRT_OBJECTS) \
		$(foreach s,$(call expand-srcs,$($1_SRCS)),$(call src-to-obj,$(s))) \
		$(call STATIC_LINK_FLAGS,$1) \
		-L$(OUTDIR) \
		$(call SHARED_LINK_FLAGS,$1)

ALL_TARGETS += $(OUTDIR)/$1

endef

$(foreach x,$(STATIC_LIBS),$(eval $(call DEFINE_STATIC_LIBRARY,$(x))))
$(foreach x,$(SHARED_LIBS),$(eval $(call DEFINE_SHARED_LIBRARY,$(x))))
$(foreach x,$(BINARIES),$(eval $(call DEFINE_BINARY,$(x))))

.PHONY: all

all: $(ALL_TARGETS)

.PHONY: clean

clean:
	@rm -rf $(OBJDIR)
	@rm -rf $(OUTDIR)

-include $(ALL_DEPS)