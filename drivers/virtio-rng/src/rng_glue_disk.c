/*
 * rng_glue_disk.c - virtiorng.device loaded from DEVS: (make DISK=1, cargo
 * feature "disk").
 *
 * One Resident, RTF_NATIVE | RTF_AUTOINIT, found by LoadSeg/InitResident
 * when a program opens virtiorng.device. devInit() opens expansion.library's
 * "pci" interface and hands over to Rust, which finds and locks the cards.
 * Each unit is set up at its first open and stopped at its last close
 * (Rust). Expunge, with nothing open, asks Rust to unlock the cards, then
 * removes the device and returns its segment list for unloading. Close
 * expunges too when an Expunge was deferred (LIBF_DELEXP).
 */

#include <proto/exec.h>
#include <proto/expansion.h>
#include <exec/types.h>
#include <exec/resident.h>
#include <exec/devices.h>
#include <exec/execbase.h>
#include <exec/io.h>
#include <exec/errors.h>
#include <dos/dos.h>   /* BPTR */

#define DEVNAME    "virtiorng.device"
#define DEVVER     1
#define DEVREV     1
#define DEVPRI     0
#define VSTRING    "virtiorng.device 1.1 (03.11.2026) disk"

/* Read by the amigaos4 crate (sys::IExec, sys::IPCI). */
struct ExecIFace *IExec = NULL;
struct PCIIFace  *IPCI  = NULL;
struct Library *ExpansionBase = NULL;

struct MyDevice {
    struct Device devNode;
    uint16        pad;
    BPTR          segList;
};

extern int32 rust_rng_init(void);
extern int32 rust_rng_expunge(void);
extern int8  rust_dev_open(struct IORequest *ior, uint32 unit, uint32 flags);
extern void  rust_dev_close(struct IORequest *ior);
extern void  rust_dev_begin_io(struct IORequest *ior);
extern void  rust_dev_abort_io(struct IORequest *ior);

/* Not a program: run from a Shell, it returns at once. */
int32 _start(void)
{
    return -1;
}

/* ---------------------------------------------------------------- device */

static uint32 _manager_Obtain(struct Interface *self)
{
    return self->Data.RefCount++;
}

static uint32 _manager_Release(struct Interface *self)
{
    return self->Data.RefCount--;
}

static void close_pci(void)
{
    if (IPCI) {
        IExec->DropInterface((struct Interface *)IPCI);
        IPCI = NULL;
    }
    if (ExpansionBase) {
        IExec->CloseLibrary(ExpansionBase);
        ExpansionBase = NULL;
    }
}

static struct MyDevice *devInit(struct MyDevice *devBase, BPTR segList,
                                struct ExecIFace *iexec)
{
    IExec = iexec;

    devBase->devNode.dd_Library.lib_Node.ln_Type = NT_DEVICE;
    devBase->devNode.dd_Library.lib_Node.ln_Pri  = DEVPRI;
    devBase->devNode.dd_Library.lib_Node.ln_Name = (STRPTR)DEVNAME;
    devBase->devNode.dd_Library.lib_Flags        = LIBF_SUMUSED | LIBF_CHANGED;
    devBase->devNode.dd_Library.lib_Version      = DEVVER;
    devBase->devNode.dd_Library.lib_Revision     = DEVREV;
    devBase->devNode.dd_Library.lib_IdString     = (STRPTR)VSTRING;
    devBase->segList                             = segList;

    IExec->DebugPrintF(DEVNAME ": init (" VSTRING ")\n");

    ExpansionBase = IExec->OpenLibrary("expansion.library", 50);
    if (ExpansionBase)
        IPCI = (struct PCIIFace *)IExec->GetInterface(ExpansionBase, "pci", 1, NULL);
    if (!IPCI) {
        IExec->DebugPrintF(DEVNAME ": cannot open expansion.library's \"pci\" interface; "
                           "device not added\n");
        close_pci();
        return NULL;
    }

    if (rust_rng_init() != 0) {
        close_pci();
        return NULL;
    }
    return devBase;
}

static APTR devExpunge(struct Interface *self);

static void devOpen(struct Interface *self, struct IORequest *ior,
                    uint32 unit, uint32 flags)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    /* Counted first, so an Expunge while the unit starts finds it open. */
    devBase->devNode.dd_Library.lib_OpenCnt++;
    int8 err = rust_dev_open(ior, unit, flags);

    ior->io_Error = err;
    if (err == 0) {
        devBase->devNode.dd_Library.lib_Flags &= ~LIBF_DELEXP;
        ior->io_Device = (struct Device *)devBase;
        ior->io_Message.mn_Node.ln_Type = NT_REPLYMSG;
    } else {
        devBase->devNode.dd_Library.lib_OpenCnt--;
        ior->io_Device = NULL;
        ior->io_Unit   = NULL;
    }
}

static APTR devClose(struct Interface *self, struct IORequest *ior)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    rust_dev_close(ior);
    ior->io_Device = NULL;
    ior->io_Unit   = NULL;
    devBase->devNode.dd_Library.lib_OpenCnt--;

    if (devBase->devNode.dd_Library.lib_OpenCnt == 0 &&
        (devBase->devNode.dd_Library.lib_Flags & LIBF_DELEXP))
        return devExpunge(self);
    return NULL;
}

static APTR devExpunge(struct Interface *self)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    if (devBase->devNode.dd_Library.lib_OpenCnt > 0 || rust_rng_expunge() == 0) {
        devBase->devNode.dd_Library.lib_Flags |= LIBF_DELEXP;
        return NULL;
    }
    close_pci();
    IExec->Remove(&devBase->devNode.dd_Library.lib_Node);
    BPTR seg = devBase->segList;
    IExec->DeleteLibrary(&devBase->devNode.dd_Library);
    return (APTR)seg;
}

static void devBeginIO(struct Interface *self __attribute__((unused)),
                       struct IORequest *ior)
{
    rust_dev_begin_io(ior);
}

static void devAbortIO(struct Interface *self __attribute__((unused)),
                       struct IORequest *ior)
{
    rust_dev_abort_io(ior);
}

static void *devManagerVectors[] = {
    (void *)_manager_Obtain,
    (void *)_manager_Release,
    (void *)0,
    (void *)0,
    (void *)devOpen,
    (void *)devClose,
    (void *)devExpunge,
    (void *)0,
    (void *)devBeginIO,
    (void *)devAbortIO,
    (void *)(APTR)-1,
};

static struct TagItem devManagerTags[] = {
    { MIT_Name,        (uint32)"__device" },
    { MIT_VectorTable, (uint32)devManagerVectors },
    { MIT_Version,     1 },
    { MIT_DataSize,    0 },
    { TAG_DONE,        0 }
};

static uint32 devInterfaces[] = {
    (uint32)devManagerTags,
    0
};

static struct TagItem devCreateTags[] = {
    { CLT_DataSize,   (uint32)sizeof(struct MyDevice) },
    { CLT_Interfaces, (uint32)devInterfaces },
    { CLT_InitFunc,   (uint32)devInit },
    { TAG_DONE,       0 }
};

/* ------------------------------------------------------------- Resident */

static const char devName[]    = DEVNAME;
static const char devVString[] = "$VER: " VSTRING;

const struct Resident DiskTag __attribute__((used, aligned(4))) = {
    RTC_MATCHWORD,
    (struct Resident *)&DiskTag,
    (APTR)(&DiskTag + 1),
    RTF_NATIVE | RTF_AUTOINIT,
    DEVVER,
    NT_DEVICE,
    DEVPRI,
    (STRPTR)devName,
    (STRPTR)(devVString + 6),
    (APTR)devCreateTags
};
