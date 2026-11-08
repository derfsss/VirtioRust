# Makefile - link virtio-scsi into a trackdisk-style block device.
# No CRT, no clib4. Run inside Docker: walkero/amigagccondocker:os4-gcc15-3.6.1
#   make              vscsirs.device    (development name, beside the C driver)
#   make DROPIN=1     virtioscsi.device (the drop-in name; build Rust with
#                                        --features dropin to match)

CC        = ppc-amigaos-gcc
SDK_INC   = /opt/ppc-amigaos/ppc-amigaos/SDK/include/include_h

ifeq ($(DROPIN),1)
TARGET    = virtioscsi.device
KICKDEF   = -DKICKSTART -DDROPIN
else
TARGET    = vscsirs.device
KICKDEF   =
endif
# KICK=1: the development name as a COLDSTART Kickstart module (test only).
ifeq ($(KICK),1)
KICKDEF   = -DKICKSTART
endif

GLUE_SRC      = src/scsi_glue.c
SYS_GLUE_SRC  = ../../amigaos4/glue/amiga_glue.c
MEM_GLUE_SRC  = ../../amigaos4/glue/freestanding_mem.c
GLUE_OBJ      = build/scsi_glue.o
SYS_GLUE_OBJ  = build/sys_glue.o
MEM_GLUE_OBJ  = build/freestanding_mem.o

RUST_LIB  = target/powerpc-unknown-amigaos/release/libvirtio_scsi.a

CFLAGS    = -O2 -Wall -D__AMIGAOS4__ -U__USE_INLINE__ -I$(SDK_INC) \
            -fno-tree-loop-distribute-patterns -ffunction-sections -fdata-sections \
            '-DDEVNAME="$(TARGET)"' $(KICKDEF)

# The Resident is found by scanning for RTC_MATCHWORD: keep it as a GC root.
LDFLAGS   = -nostartfiles -nodefaultlibs -Wl,-z,noexecstack -Wl,--gc-sections \
            -Wl,--undefined=ScsiTag
LIBS      = -lgcc

.PHONY: all clean

all: build $(TARGET)

build:
	mkdir -p build

$(GLUE_OBJ): $(GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(MEM_GLUE_OBJ): $(MEM_GLUE_SRC)
	$(CC) $(CFLAGS) -ffreestanding -fno-builtin -c $< -o $@

$(SYS_GLUE_OBJ): $(SYS_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(TARGET): $(GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB)
	$(CC) $(LDFLAGS) -o $@ $(GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB) $(LIBS)
	ppc-amigaos-strip $@

clean:
	rm -rf build vscsirs.device virtioscsi.device
