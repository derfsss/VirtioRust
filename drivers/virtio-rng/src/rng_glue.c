/*
 * rng_glue.c - virtiorng.device, an interrupt-driven virtio-rng driver in
 * Rust.
 *
 * A kickstart module with two Residents in one object (KickTags):
 *
 *   KickTags.cold   RTF_NATIVE | RTF_COLDSTART | RTF_AUTOINIT, NT_DEVICE.
 *                   devInit() opens expansion.library's "pci" interface and
 *                   hands over to Rust, which finds the virtio-rng PCI
 *                   functions, sets up their queues and reads a first sample.
 *                   If there is no device, the init fails and exec does not
 *                   add virtiorng.device.
 *
 *   KickTags.after  RTF_NATIVE | RTF_AFTERDOS. Once DOS is up, Rust opens
 *                   each unit by name and checks its reads.
 *
 * Each unit answers CMD_READ with random bytes, CMD_RESET and CMD_FLUSH abort
 * queued and current requests, NSCMD_DEVICEQUERY lists the commands, and
 * AbortIO ends a queued or current read. Everything is reported with
 * DebugPrintF to the debug output (serial).
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
#define DEVREV     0
#define DEVPRI     0
#define VSTRING    "virtiorng.device 1.0 (03.11.2026)"
#define AFTERPRI   -100

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
extern void  rust_rng_afterdos(void);
extern int8  rust_dev_open(struct IORequest *ior, uint32 unit, uint32 flags);
extern void  rust_dev_close(struct IORequest *ior);
extern void  rust_dev_begin_io(struct IORequest *ior);
extern void  rust_dev_abort_io(struct IORequest *ior);

struct KickTags {
    struct Resident cold;
    struct Resident after;
};
extern const struct KickTags KickTags;

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

    IExec->DebugPrintF(DEVNAME ": COLDSTART init (" VSTRING ")\n");

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

static void devOpen(struct Interface *self, struct IORequest *ior,
                    uint32 unit, uint32 flags)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;
    int8 err = rust_dev_open(ior, unit, flags);

    ior->io_Error = err;
    if (err == 0) {
        devBase->devNode.dd_Library.lib_OpenCnt++;
        devBase->devNode.dd_Library.lib_Flags &= ~LIBF_DELEXP;
        ior->io_Device = (struct Device *)devBase;
        ior->io_Message.mn_Node.ln_Type = NT_REPLYMSG;
    } else {
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
    return NULL;   /* a kickstart module is never unloaded */
}

static APTR devExpunge(struct Interface *self)
{
    struct MyDevice *devBase = (struct MyDevice *)self->Data.LibBase;

    devBase->devNode.dd_Library.lib_Flags |= LIBF_DELEXP;
    return NULL;   /* a kickstart module is never unloaded */
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

/* ------------------------------------------------------------- AFTERDOS */

static APTR afterDosInit(APTR dummy __attribute__((unused)),
                         BPTR seglist __attribute__((unused)),
                         struct ExecBase *sysbase)
{
    IExec = (struct ExecIFace *)sysbase->MainInterface;
    rust_rng_afterdos();
    return NULL;
}

/* ------------------------------------------------------------ Residents */

static const char devName[]    = DEVNAME;
static const char devVString[] = VSTRING;
static const char afterName[]  = "virtiorng.afterdos";

/* One object holds both tags, so the COLDSTART tag is first by layout. */
const struct KickTags KickTags __attribute__((used, aligned(4), section(".rodata"))) = {
    {
        RTC_MATCHWORD,
        (struct Resident *)&KickTags.cold,
        (APTR)(&KickTags.cold + 1),
        RTF_NATIVE | RTF_COLDSTART | RTF_AUTOINIT,
        DEVVER,
        NT_DEVICE,
        DEVPRI,
        (STRPTR)devName,
        (STRPTR)devVString,
        (APTR)devCreateTags
    },
    {
        RTC_MATCHWORD,
        (struct Resident *)&KickTags.after,
        (APTR)(&KickTags.after + 1),
        RTF_NATIVE | RTF_AFTERDOS,
        DEVVER,
        NT_UNKNOWN,
        AFTERPRI,
        (STRPTR)afterName,
        (STRPTR)devVString,
        (APTR)afterDosInit
    }
};

/* Not a program: running it from the Shell does nothing. */
int32 _start(void)
{
    return RETURN_FAIL;
}
