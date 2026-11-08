# Makefile — Link Rust staticlib + C glue into an AmigaOS handler/driver
# No CRT, no clib4, no -lauto. Driver mode: -nostartfiles -nodefaultlibs -lgcc
# Run inside Docker: walkero/amigagccondocker:os4-gcc15-3.6.1

TARGET    = s2test
CC        = ppc-amigaos-gcc

# SDK include paths
SDK_INC   = /opt/ppc-amigaos/ppc-amigaos/SDK/include/include_h

# C glue sources
DRIVER_GLUE_SRC = src/driver_glue.c
SYS_GLUE_SRC    = ../../amigaos4/glue/amiga_glue.c
MEM_GLUE_SRC    = ../../amigaos4/glue/freestanding_mem.c

# C glue objects
DRIVER_GLUE_OBJ = build/driver_glue.o
SYS_GLUE_OBJ    = build/sys_glue.o
MEM_GLUE_OBJ    = build/freestanding_mem.o

# Rust output (staticlib built on host)
RUST_LIB  = target/powerpc-unknown-amigaos/release/libs2test.a

# Compiler flags — NO CRT, driver-compatible
# -ffunction-sections/-fdata-sections + --gc-sections strips unused glue
CFLAGS    = -O2 -Wall -D__AMIGAOS4__ -U__USE_INLINE__ -I$(SDK_INC) \
            -fno-tree-loop-distribute-patterns -ffunction-sections -fdata-sections

# Linker flags — driver mode (no CRT, no default libs)
LDFLAGS   = -nostartfiles -nodefaultlibs -Wl,-z,noexecstack -Wl,--gc-sections
LIBS      = -lgcc

.PHONY: all clean

all: build $(TARGET)

build:
	mkdir -p build

$(DRIVER_GLUE_OBJ): $(DRIVER_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(MEM_GLUE_OBJ): $(MEM_GLUE_SRC)
	$(CC) $(CFLAGS) -ffreestanding -fno-builtin -c $< -o $@

$(SYS_GLUE_OBJ): $(SYS_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(TARGET): $(DRIVER_GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB)
	$(CC) $(LDFLAGS) -o $@ $(DRIVER_GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB) $(LIBS)
	ppc-amigaos-strip $@

clean:
	rm -rf build $(TARGET)
