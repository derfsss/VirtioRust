# Makefile - link virtio-net into virtionet.device, a SANA-II network
# driver loaded from DEVS:Networks/. No CRT, no clib4.
# Run inside Docker: walkero/amigagccondocker:os4-gcc15-3.6.1

TARGET    = virtionet.device
CC        = ppc-amigaos-gcc
SDK_INC   = /opt/ppc-amigaos/ppc-amigaos/SDK/include/include_h

GLUE_SRC      = src/net_glue.c
SYS_GLUE_SRC  = ../../amigaos4/glue/amiga_glue.c
MEM_GLUE_SRC  = ../../amigaos4/glue/freestanding_mem.c
GLUE_OBJ      = build/net_glue.o
SYS_GLUE_OBJ  = build/sys_glue.o
MEM_GLUE_OBJ  = build/freestanding_mem.o

RUST_LIB  = target/powerpc-unknown-amigaos/release/libvirtio_net.a

CFLAGS    = -O2 -Wall -D__AMIGAOS4__ -U__USE_INLINE__ -I$(SDK_INC) \
            -fno-tree-loop-distribute-patterns -ffunction-sections -fdata-sections

# The Resident is found by scanning for RTC_MATCHWORD: keep it as a GC root.
LDFLAGS   = -nostartfiles -nodefaultlibs -Wl,-z,noexecstack -Wl,--gc-sections \
            -Wl,--undefined=NetTag
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
	rm -rf build $(TARGET)
