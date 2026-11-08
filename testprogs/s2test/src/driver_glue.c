/*
 * driver_glue.c - entry point for s2test.
 *
 * s2test has no C runtime (-nostartfiles -nodefaultlibs -lgcc). _start()
 * gets ExecBase from the system, sets IExec, opens dos.library's main
 * interface for Shell output, and passes the argument string to Rust.
 */

#define __NOLIBBASE__
#define __NOGLOBALIFACE__
#include <proto/exec.h>
#include <proto/dos.h>
#include <exec/types.h>

/* Read by the amigaos4 crate (sys::IExec, sys::IDOS). */
struct ExecIFace *IExec = NULL;
struct DOSIFace  *IDOS  = NULL;

#ifndef GIT_HASH
#define GIT_HASH "unknown"
#endif
/* The commit s2test was built from (tools/build-package.sh passes it), so
 * a copy shipped in another project's archive can be traced. */
const char build_banner[] = "s2test build " GIT_HASH;

extern int32 rust_main(STRPTR args, int32 arglen);

int32 _start(STRPTR argstring, int32 arglen, struct ExecBase *sysbase)
{
    struct Library *DOSBase;
    int32 ret = RETURN_FAIL;

    IExec = (struct ExecIFace *)sysbase->MainInterface;
    (void)*(volatile const char *)build_banner;   /* keep it linked */
    IExec->Obtain();

    DOSBase = IExec->OpenLibrary("dos.library", 52);
    if (DOSBase) {
        IDOS = (struct DOSIFace *)IExec->GetInterface(DOSBase, "main", 1, NULL);
        if (IDOS) {
            ret = rust_main(argstring, arglen);
            IExec->DropInterface((struct Interface *)IDOS);
        }
        IExec->CloseLibrary(DOSBase);
    }

    IExec->Release();
    return ret;
}
