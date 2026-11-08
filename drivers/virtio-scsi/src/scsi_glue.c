/*
 * scsi_glue.c - the Rust virtioscsi.device, a trackdisk-style block device
 * for QEMU's virtio-scsi disks. Two builds (Makefile):
 *
 *   make          vscsirs.device, loaded from DEVS: beside the C
 *                 virtioscsi.device that runs the system disk
 *                 (RTF_NATIVE | RTF_AUTOINIT);
 *   make DROPIN=1 virtioscsi.device, the Kickstart module that replaces
 *                 the C driver (RTF_COLDSTART too, KICKSTART defined).
 *
 * As in every shipping disk kmod and the C driver (device.c:53-97): the
 * Resident is in writable .data, not const; lib_Version agrees with the
 * IdString (SFS checks: 1.1 for the drop-in, 53.x for the development
 * build); priority 0; CLT_Vector68K gives a 68k jump table
 * with BeginIO at -30 and AbortIO at -36, which SFS 1.290 calls; no
 * AddDevice and no mounter.library call (diskboot.kmod makes the DOSNodes
 * from diskboot.config's "virtioscsi.device 8 3").
 *
 * devInit() opens expansion.library's "pci" interface and utility.library,
 * then hands over to Rust, which finds and locks the controllers. Expunge,
 * with nothing open, asks Rust to let them go, then removes the device and
 * returns its segment list. Close runs a deferred expunge through Expunge.
 */

#include <proto/exec.h>
#include <proto/expansion.h>
#include <proto/utility.h>
#include <exec/types.h>
#include <exec/resident.h>
#include <exec/devices.h>
#include <exec/execbase.h>
#include <exec/io.h>
#include <exec/errors.h>
#include <dos/dos.h>   /* BPTR */

#ifndef DEVNAME
#define DEVNAME    "vscsirs.device"
#endif
/* lib_Version, rt_Version and the IdString must agree (SFS checks
 * lib_Version against the IdString). The Kickstart drop-in is 1.1; the
 * development build keeps the C driver's 53.x line. */
#ifdef DROPIN
#define DEVVER     1
#define DEVREV     1
#define VSTRING    DEVNAME " 1.1 (05.11.2026)"
#else
#define DEVVER     53
#define DEVREV     0
#define VSTRING    DEVNAME " 53.1 (04.11.2026)"
#endif
#define DEVPRI     0

/* Read by the amigaos4 crate (sys::IExec, sys::IPCI, sys::IUtility). */
struct ExecIFace    *IExec    = NULL;
struct PCIIFace     *IPCI     = NULL;
struct UtilityIFace *IUtility = NULL;
struct Library *ExpansionBase = NULL;
struct Library *UtilityBase   = NULL;

struct MyDevice {
    struct Device devNode;
    uint16        pad;
    BPTR          segList;
};

extern int32 rust_scsi_init(void);
extern int32 rust_scsi_expunge(void);
extern int8  rust_dev_open(struct IORequest *ior, uint32 unit, uint32 flags);
extern void  rust_dev_close(struct IORequest *ior);
extern void  rust_dev_begin_io(struct IORequest *ior);
extern void  rust_dev_abort_io(struct IORequest *ior);

/* Not a program: run from a Shell, it returns at once. */
int32 _start(void)
{
    return -1;
}

static uint32 _manager_Obtain(struct Interface *self)
{
    return self->Data.RefCount++;
}

static uint32 _manager_Release(struct Interface *self)
{
    return self->Data.RefCount--;
}

static void close_libs(void)
{
    if (IPCI) {
        IExec->DropInterface((struct Interface *)IPCI);
        IPCI = NULL;
    }
    if (ExpansionBase) {
        IExec->CloseLibrary(ExpansionBase);
        ExpansionBase = NULL;
    }
    if (IUtility) {
        IExec->DropInterface((struct Interface *)IUtility);
        IUtility = NULL;
    }
    if (UtilityBase) {
        IExec->CloseLibrary(UtilityBase);
        UtilityBase = NULL;
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
    UtilityBase = IExec->OpenLibrary("utility.library", 50);
    if (UtilityBase)
        IUtility = (struct UtilityIFace *)IExec->GetInterface(UtilityBase, "main", 1, NULL);
    if (!IPCI || !IUtility) {
        IExec->DebugPrintF(DEVNAME ": cannot open expansion.library \"pci\" or utility.library; "
                           "device not added\n");
        close_libs();
        return NULL;
    }

    if (rust_scsi_init() != 0) {
        close_libs();
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

    if (devBase->devNode.dd_Library.lib_OpenCnt > 0 || rust_scsi_expunge() == 0) {
        devBase->devNode.dd_Library.lib_Flags |= LIBF_DELEXP;
        return NULL;
    }
    close_libs();
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

/* The 68k jump table, offsets -6 .. -36 in order (MakeFunctions; -1 ends
 * it). Without it BeginIO and AbortIO are absent from the 68k side and a
 * 68k handler's jsr -30(a6) lands in unmapped memory (the C driver's
 * finding, device.c:36-52). */
static APTR devVectors68K[] = {
    (APTR)devOpen,     /* -6  */
    (APTR)devClose,    /* -12 */
    (APTR)devExpunge,  /* -18 */
    (APTR)NULL,        /* -24 reserved */
    (APTR)devBeginIO,  /* -30 */
    (APTR)devAbortIO,  /* -36 */
    (APTR)-1,
};

static struct TagItem devCreateTags[] = {
    { CLT_DataSize,      (uint32)sizeof(struct MyDevice) },
    { CLT_Interfaces,    (uint32)devInterfaces },
    { CLT_InitFunc,      (uint32)devInit },
    { CLT_Vector68K,     (uint32)devVectors68K },
    { CLT_NoLegacyIFace, FALSE },
    { TAG_DONE,          0 }
};

static const char devName[]    = DEVNAME;
static const char devVString[] = "$VER: " VSTRING;

#ifdef KICKSTART
#define RESIDENT_FLAGS (RTF_NATIVE | RTF_COLDSTART | RTF_AUTOINIT)
#else
#define RESIDENT_FLAGS (RTF_NATIVE | RTF_AUTOINIT)
#endif

/* Not const: in .data, as every working disk kmod has it. */
struct Resident ScsiTag __attribute__((used, aligned(4))) = {
    RTC_MATCHWORD,
    (struct Resident *)&ScsiTag,
    (APTR)(&ScsiTag + 1),
    RESIDENT_FLAGS,
    DEVVER,
    NT_DEVICE,
    DEVPRI,
    (STRPTR)devName,
    (STRPTR)(devVString + 6),
    (APTR)devCreateTags
};
