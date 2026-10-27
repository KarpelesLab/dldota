/* dldota: load system shared libraries from a statically linked program.
 * Link with libdldota.a. See docs/design.md. */
#ifndef DLDOTA_H
#define DLDOTA_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct dldota_ctx dldota_ctx;

/* dlopen() flags; identical values in glibc and musl. */
#define DLDOTA_LAZY     0x0001
#define DLDOTA_NOW      0x0002
#define DLDOTA_NOLOAD   0x0004
#define DLDOTA_LOCAL    0x0000
#define DLDOTA_GLOBAL   0x0100
#define DLDOTA_NODELETE 0x1000

/* Boot the system dynamic loader. interpreter may be NULL (auto-detect).
 * Returns NULL on error; see dldota_error(). */
dldota_ctx *dldota_ctx_new(const char *interpreter);

/* Release the handle WITHOUT unloading: the foreign world is leaked. */
void dldota_ctx_free(dldota_ctx *ctx);

/* Unload: run foreign destructors and unmap everything, foreign heap included.
 * Returns 0 and frees ctx on success; -1 with ctx still valid on failure
 * (e.g. threads created by foreign code still running). */
int dldota_ctx_unload(dldota_ctx *ctx);

/* Every allocation made by foreign code goes through a per-context heap. */
typedef struct dldota_heap_usage {
    size_t live_allocations; /* allocations not yet freed */
    size_t live_bytes;       /* bytes requested by those allocations */
    size_t mapped_bytes;     /* address space mapped by the allocator */
} dldota_heap_usage;

void dldota_heap_usage(const dldota_ctx *ctx, dldota_heap_usage *out);

/* dlopen/dlsym/dlclose equivalents. Pointers returned by dldota_sym() can be
 * called directly: they switch the thread pointer around the call. */
void *dldota_open(dldota_ctx *ctx, const char *name, int flags);
void *dldota_sym(dldota_ctx *ctx, void *handle, const char *name);
int dldota_close(dldota_ctx *ctx, void *handle);

/* Wrap a host function so foreign code can call it (e.g. a qsort comparator):
 * the returned pointer switches the thread pointer back to the host around
 * the call. Pass it to foreign code instead of func. NULL on error. */
void *dldota_callback(dldota_ctx *ctx, void *func);
void dldota_callback_free(dldota_ctx *ctx, void *thunk);

/* Last error on the calling thread, or NULL. Reading clears it. */
const char *dldota_error(void);

#ifdef __cplusplus
}
#endif

#endif /* DLDOTA_H */
