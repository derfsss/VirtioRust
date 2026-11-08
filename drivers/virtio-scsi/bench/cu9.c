/* cu9: the C driver suite's TEST 9 on its own (two processes, 200 DoIO
 * reads each on units U0 and U1, at once), repeated, with NP_Child as an
 * argument, to tell a driver fault from a DOS child-exit race.
 *   cu9 <rounds> <npchild 0|1> <u0> <u1> <children 1|2> <device> [reads1] [childpri] [mode 0|1|2]
 * Unit 99: the child opens nothing and exits at once. childpri (default 0)
 * above the caller's priority is what exposes the DOS race: CreateNewProc
 * links the new process into DOS's list only after AddTaskTags returns, so
 * a child that runs and exits before then is removed unlinked (a DSI in
 * Remove()). Children below the caller's priority are safe.
 */
#include <exec/types.h>
#include <exec/io.h>
#include <dos/dos.h>
#include <dos/dostags.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <stdio.h>
#include <stdlib.h>

static const char *devname = "virtioscsi.device";
struct Arg {
    ULONG unit, reads, errors;
    volatile BOOL done;
    struct Task *parent;
    ULONG sigbit;
    /* Stack-base watch: the iteration at which the first 32 bytes at
     * tc_SPLower changed (-1: never), and the bytes before and after. */
    LONG changed_at;
    ULONG lower, before[8], after[8];
};

/* mode (argv[9], default 2): 0 = the C suite's way (void child(void), no handshake);
 * 1 = the documented _start prototype (dos.doc CreateNewProc, NP_Start);
 * 2 = that plus a start-up handshake: the child first waits for CTRL-F,
 *     which the parent sends only after CreateNewProcTags() has returned
 *     (the wiki's Exec Signals example, "Wait for a startup signal"). */
static int mode = 0;

static void child_body(void);

static void child(void)
{
    child_body();
}

static int32 child_start(STRPTR args, int32 arglen, struct ExecBase *sysbase)
{
    (void)args; (void)arglen; (void)sysbase;
    if (mode == 2)
        IExec->Wait(SIGBREAKF_CTRL_F);
    child_body();
    return RETURN_OK;
}

static void child_body(void)
{
    struct Task *self = IExec->FindTask(NULL);
    struct Arg *a = (struct Arg *)self->tc_UserData;
    struct MsgPort *port = IExec->AllocSysObjectTags(ASOT_PORT, TAG_DONE);
    struct IOStdReq *req = (struct IOStdReq *)IExec->AllocSysObjectTags(ASOT_IOREQUEST,
        ASOIOR_Size, sizeof(struct IOStdReq), ASOIOR_ReplyPort, port, TAG_DONE);
    ULONG errors = 0;
    if (a->unit == 99) {
        /* No device at all: the child exits at once, without blocking. */
        errors = 0;
    } else if (port && req && IExec->OpenDevice(devname, a->unit, (struct IORequest *)req, 0) == 0) {
        UBYTE *buf = IExec->AllocVecTags(512, AVT_Type, MEMF_SHARED, AVT_ClearWithValue, 0, TAG_DONE);
        if (buf) {
            /* Watch 64 bytes from 32 below tc_SPLower (the kernel's node may
             * sit just before the stack proper). */
            volatile ULONG *low = (volatile ULONG *)self->tc_SPLower; /* below it: unmapped */
            ULONG w0[16];
            a->lower = (ULONG)self->tc_SPLower;
            for (int k = 0; k < 16; k++) w0[k] = low[k];
            for (int k = 0; k < 8; k++) a->before[k] = low[k];
            IExec->DebugPrintF("cu9 unit %ld: SPLower 0x%08lx SPUpper 0x%08lx buf 0x%08lx req 0x%08lx port 0x%08lx; watch %08lx %08lx %08lx %08lx | %08lx %08lx %08lx %08lx\n",
                a->unit, (ULONG)self->tc_SPLower, (ULONG)self->tc_SPUpper, (ULONG)buf, (ULONG)req, (ULONG)port,
                w0[0], w0[1], w0[2], w0[3], w0[8], w0[9], w0[10], w0[11]);
            for (ULONG i = 0; i < a->reads; i++) {
                req->io_Command = CMD_READ;
                req->io_Data = buf;
                req->io_Length = 512;
                req->io_Offset = (i % 64) * 512;
                IExec->DoIO((struct IORequest *)req);
                if (req->io_Error) errors++;
                if (a->changed_at < 0) {
                    for (int k = 0; k < 16; k++) {
                        if (low[k] != w0[k]) {
                            a->changed_at = (LONG)i;
                            IExec->DebugPrintF("cu9 unit %ld: WATCH CHANGED after read %ld (offset %ld), long %ld: %08lx -> %08lx; now %08lx %08lx %08lx %08lx | %08lx %08lx %08lx %08lx\n",
                                a->unit, (LONG)i, (LONG)((i % 64) * 512), (LONG)k, w0[k], low[k],
                                low[0], low[1], low[2], low[3], low[8], low[9], low[10], low[11]);
                            break;
                        }
                    }
                }
            }
            IExec->DebugPrintF("cu9 unit %ld: reads done, errors %ld, watch %s\n", a->unit, errors, a->changed_at < 0 ? "unchanged" : "CHANGED");
            IExec->FreeVec(buf);
        }
        IExec->CloseDevice((struct IORequest *)req);
    } else {
        errors = 9999;
    }
    a->errors = errors;
    a->done = TRUE;
    if (req) IExec->FreeSysObject(ASOT_IOREQUEST, req);
    if (port) IExec->FreeSysObject(ASOT_PORT, port);
    IExec->Signal(a->parent, 1UL << a->sigbit);
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 5;
    int npchild = argc > 2 ? atoi(argv[2]) : 1;
    ULONG u0 = argc > 4 ? (ULONG)atoi(argv[3]) : 0, u1 = argc > 4 ? (ULONG)atoi(argv[4]) : 1;
    int children = argc > 5 ? atoi(argv[5]) : 2;
    if (argc > 6) devname = argv[6];
    ULONG reads1 = argc > 7 ? (ULONG)atoi(argv[7]) : 200;
    LONG childpri = argc > 8 ? atoi(argv[8]) : 0;
    mode = argc > 9 ? atoi(argv[9]) : 2; /* the handshake unless asked otherwise */
    ULONG entry = mode == 0 ? (ULONG)child : (ULONG)child_start;
    int bad = 0;
    for (int r = 0; r < rounds; r++) {
        LONG s0 = IExec->AllocSignal(-1), s1 = IExec->AllocSignal(-1);
        struct Arg a0 = { u0, 200, 0, FALSE, IExec->FindTask(NULL), (ULONG)s0, -1, 0, {0}, {0} };
        struct Arg a1 = { u1, reads1, 0, FALSE, IExec->FindTask(NULL), (ULONG)s1, -1, 0, {0}, {0} };
        struct Process *p0 = IDOS->CreateNewProcTags(NP_Entry, entry, NP_Name, (ULONG)"cu9_0",
            NP_Priority, childpri, NP_UserData, (ULONG)&a0, NP_Child, npchild, TAG_DONE);
        /* mode 2: in the wait for CTRL-F the child cannot exit before DOS has
         * linked it, so it may go now that CreateNewProcTags() has returned. */
        if (mode == 2 && p0)
            IExec->Signal((struct Task *)p0, SIGBREAKF_CTRL_F);
        struct Process *p1 = (struct Process *)1;
        if (children > 1)
            p1 = IDOS->CreateNewProcTags(NP_Entry, entry, NP_Name, (ULONG)"cu9_1",
                NP_Priority, childpri, NP_UserData, (ULONG)&a1, NP_Child, npchild, TAG_DONE);
        if (mode == 2 && children > 1 && p1)
            IExec->Signal((struct Task *)p1, SIGBREAKF_CTRL_F);
        if (children <= 1)
            a1.done = TRUE;
        ULONG want = (1UL << s0) | (children > 1 ? (1UL << s1) : 0), got = 0;
        for (int t = 0; t < 600 && got != want; t++) {
            got |= IExec->SetSignal(0, want) & want;
            if (got != want) IDOS->Delay(5);
        }
        /* Let both children leave DOS before their Arg goes. */
        IDOS->Delay(25);
        int ok = p0 && p1 && a0.done && a1.done && a0.errors == 0 && a1.errors == 0;
        bad += !ok;
        printf("round %d: npchild %d units %lu/%lu: %s (errors %lu/%lu, done %d/%d)\n", r + 1, npchild,
               u0, u1, ok ? "ok" : "FAIL", a0.errors, a1.errors, a0.done, a1.done);
        struct Arg *as[2] = { &a0, &a1 };
        for (int c = 0; c < 2; c++) {
            struct Arg *x = as[c];
            if (x->changed_at >= 0) {
                printf("  child %d: STACK BASE 0x%08lx CHANGED after read %ld (offset %ld)\n", c, x->lower, x->changed_at, (x->changed_at % 64) * 512);
                printf("    before:"); for (int k = 0; k < 8; k++) printf(" %08lx", x->before[k]); printf("\n");
                printf("    after: "); for (int k = 0; k < 8; k++) printf(" %08lx", x->after[k]); printf("\n");
                IExec->DebugPrintF("cu9 child %ld: stack base 0x%08lx changed after read %ld: %08lx %08lx %08lx %08lx -> %08lx %08lx %08lx %08lx\n",
                    (LONG)c, x->lower, x->changed_at, x->before[0], x->before[1], x->before[2], x->before[3], x->after[0], x->after[1], x->after[2], x->after[3]);
            }
        }
        IExec->FreeSignal(s0);
        IExec->FreeSignal(s1);
    }
    printf("cu9: %d/%d rounds ok\n", rounds - bad, rounds);
    return bad ? 20 : 0;
}
