# Makefile - link virtio-rng into virtiorng.device: a kickstart module, or
# with DISK=1 (cargo feature "disk") a device loaded from DEVS:, written to
# disk/virtiorng.device.
# No CRT, no clib4: -nostartfiles -nodefaultlibs -lgcc.
# Run inside Docker: walkero/amigagccondocker:os4-gcc15-3.6.1

DISK      ?= 0
ifeq ($(DISK),1)
TARGET    = disk/virtiorng.device
KICK_GLUE_SRC   = src/rng_glue_disk.c
GC_ROOT   = DiskTag
else
TARGET    = virtiorng.device
KICK_GLUE_SRC   = src/rng_glue.c
GC_ROOT   = KickTags
endif
CC        = ppc-amigaos-gcc

SDK_INC   = /opt/ppc-amigaos/ppc-amigaos/SDK/include/include_h

SYS_GLUE_SRC    = ../../amigaos4/glue/amiga_glue.c
MEM_GLUE_SRC    = ../../amigaos4/glue/freestanding_mem.c

KICK_GLUE_OBJ   = build/rng_glue.o
SYS_GLUE_OBJ    = build/sys_glue.o
MEM_GLUE_OBJ    = build/freestanding_mem.o

RUST_LIB  = target/powerpc-unknown-amigaos/release/libvirtio_rng.a

CFLAGS    = -O2 -Wall -D__AMIGAOS4__ -U__USE_INLINE__ -I$(SDK_INC) \
            -fno-tree-loop-distribute-patterns -ffunction-sections -fdata-sections

# Both Residents (one object, KickTags) are found by scanning for RTC_MATCHWORD,
# so nothing references them: keep KickTags as a GC root. rng_glue.o comes
# first so the COLDSTART tag is the first Resident in the file.
LDFLAGS   = -nostartfiles -nodefaultlibs -Wl,-z,noexecstack -Wl,--gc-sections \
            -Wl,--undefined=$(GC_ROOT)
LIBS      = -lgcc

.PHONY: all clean

all: build $(TARGET)

build:
	mkdir -p build disk

$(KICK_GLUE_OBJ): $(KICK_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(MEM_GLUE_OBJ): $(MEM_GLUE_SRC)
	$(CC) $(CFLAGS) -ffreestanding -fno-builtin -c $< -o $@

$(SYS_GLUE_OBJ): $(SYS_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(TARGET): $(KICK_GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB)
	$(CC) $(LDFLAGS) -o $@ $(KICK_GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB) $(LIBS)
	ppc-amigaos-strip $@

clean:
	rm -rf build $(TARGET)
