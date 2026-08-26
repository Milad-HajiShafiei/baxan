/*
 * baxan-tracker: intercepts heap allocations and emits Baxan JSONL events.
 *
 * Linux:  LD_PRELOAD + dlsym(RTLD_NEXT) — no target changes required.
 * macOS:  fishhook-style rebinding of the indirect symbol pointers in every
 *         loaded image (__got and __la_symbol_ptr), driven by the Mach-O
 *         indirect symbol table.  No __interpose section, no target rebuild,
 *         and no bootstrap buffer — so libc/objc internals are never fed
 *         pointers that malloc_size() can't understand.
 *
 * The emit path uses raw write() and stack snprintf() so it never calls
 * malloc (which would recurse into the tracker).
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdint.h>
#include <time.h>

/* ------------------------------------------------------------------ */
/* Shared state                                                        */
/* ------------------------------------------------------------------ */

static int          tracker_fd = -1;
static int          file_init  = 0;
static size_t       seq        = 0;
static volatile int emit_lock  = 0;

static void lock_emit(void) {
    while (__sync_lock_test_and_set(&emit_lock, 1)) {
    }
}
static void unlock_emit(void) {
    __sync_lock_release(&emit_lock);
}

static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000;
}

static void ensure_file(void) {
    if (file_init) return;
    file_init = 1;
    const char *path = getenv("BAXAN_TRACKER_OUTPUT");
    if (path && path[0])
        tracker_fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
}

static void emit_declare(const char *id, const char *val,
                          size_t bytes, const char *addr) {
    ensure_file();
    if (tracker_fd < 0) return;
    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "{\"seq\":%zu,\"time_ms\":%lu,\"kind\":\"declare\","
        "\"id\":\"%s\",\"name\":\"%s\",\"type_name\":\"heap\","
        "\"value\":\"%s\",\"storage\":\"heap\",\"zone\":\"heap\","
        "\"address\":\"%s\",\"bytes\":%zu,\"thread\":\"main\"}\n",
        ++seq, (unsigned long)now_ms(), id, id, val, addr, bytes);
    if (n > 0) {
        lock_emit();
        write(tracker_fd, buf, (size_t)n);
        unlock_emit();
    }
}

static void emit_drop(const char *id, const char *addr) {
    ensure_file();
    if (tracker_fd < 0) return;
    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "{\"seq\":%zu,\"time_ms\":%lu,\"kind\":\"drop\","
        "\"id\":\"%s\",\"name\":\"%s\",\"type_name\":\"heap\","
        "\"value\":\"<dropped>\",\"storage\":\"heap\",\"zone\":\"heap\","
        "\"address\":\"%s\",\"bytes\":0,\"thread\":\"main\"}\n",
        ++seq, (unsigned long)now_ms(), id, id, addr);
    if (n > 0) {
        lock_emit();
        write(tracker_fd, buf, (size_t)n);
        unlock_emit();
    }
}

/* ================================================================== */
/* macOS: fishhook-style rebinding                                     */
/* ================================================================== */
#ifdef __APPLE__
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach-o/nlist.h>
#include <mach/mach.h>
#include <mach/vm_map.h>

typedef struct mach_header_64     mach_header_t;
typedef struct segment_command_64 segment_command_t;
typedef struct section_64         section_t;
typedef struct nlist_64           nlist_t;
#define LC_SEGMENT_ARCH_DEPENDENT LC_SEGMENT_64
#define SEG_DATA_CONST            "__DATA_CONST"

/* --- tracked allocator functions (call the real ones) --- */

static void *(*real_malloc)(size_t)          = NULL;
static void  (*real_free)(void *)            = NULL;
static void *(*real_calloc)(size_t, size_t)  = NULL;
static void *(*real_realloc)(void *, size_t) = NULL;

static void *track_malloc(size_t sz) {
    void *p = real_malloc(sz);
    if (p) {
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        snprintf(val,  sizeof(val),  "%zuB", sz);
        emit_declare(id, val, sz, addr);
    }
    return p;
}

static void track_free(void *p) {
    if (!p) return;
    {
        char id[24], addr[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        emit_drop(id, addr);
    }
    real_free(p);
}

static void *track_calloc(size_t n, size_t s) {
    void *p = real_calloc(n, s);
    if (p) {
        size_t total = n * s;
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        snprintf(val,  sizeof(val),  "%zuB", total);
        emit_declare(id, val, total, addr);
    }
    return p;
}

static void *track_realloc(void *p, size_t sz) {
    if (p) {
        char id[24], addr[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        emit_drop(id, addr);
    }
    void *np = real_realloc(p, sz);
    if (np) {
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", np);
        snprintf(addr, sizeof(addr), "%p",   np);
        snprintf(val,  sizeof(val),  "%zuB", sz);
        emit_declare(id, val, sz, addr);
    }
    return np;
}

/* --- rebinding (64-bit) --- */

static void perform_rebinding(section_t *sect, intptr_t slide,
                              nlist_t *symtab, char *strtab,
                              uint32_t *indirect_symtab) {
    uint32_t *indices = indirect_symtab + sect->reserved1;
    void **bindings = (void **)((uintptr_t)slide + sect->addr);
    for (uint32_t i = 0; i < sect->size / sizeof(void *); i++) {
        uint32_t idx = indices[i];
        if (idx == INDIRECT_SYMBOL_ABS || idx == INDIRECT_SYMBOL_LOCAL ||
            idx == (INDIRECT_SYMBOL_LOCAL | INDIRECT_SYMBOL_ABS))
            continue;
        char *name = strtab + symtab[idx].n_un.n_strx;
        if (!name[0] || !name[1]) continue;
        name++; /* skip leading underscore */

        void *replacement = NULL;
        void **replaced = NULL;
        if (!strcmp(name, "malloc")) {
            replacement = (void *)track_malloc;
            replaced = (void **)&real_malloc;
        } else if (!strcmp(name, "free")) {
            replacement = (void *)track_free;
            replaced = (void **)&real_free;
        } else if (!strcmp(name, "calloc")) {
            replacement = (void *)track_calloc;
            replaced = (void **)&real_calloc;
        } else if (!strcmp(name, "realloc")) {
            replacement = (void *)track_realloc;
            replaced = (void **)&real_realloc;
        } else {
            continue;
        }

        /* save the original before overwriting */
        if (replaced && bindings[i] != replacement)
            *replaced = bindings[i];

        kern_return_t err = vm_protect(mach_task_self(),
                                       (uintptr_t)bindings, sect->size, 0,
                                       VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY);
        if (err == KERN_SUCCESS)
            bindings[i] = replacement;
    }
}

static void rebind_image(const struct mach_header *header, intptr_t slide) {
    Dl_info info;
    if (dladdr(header, &info) == 0) return;

    segment_command_t *linkedit = NULL;
    struct symtab_command *symtab_cmd = NULL;
    struct dysymtab_command *dysymtab_cmd = NULL;

    uintptr_t cur = (uintptr_t)header + sizeof(mach_header_t);
    for (uint32_t i = 0; i < header->ncmds; i++) {
        struct load_command *lc = (struct load_command *)cur;
        if (lc->cmd == LC_SEGMENT_ARCH_DEPENDENT) {
            segment_command_t *seg = (segment_command_t *)lc;
            if (strcmp(seg->segname, SEG_LINKEDIT) == 0)
                linkedit = seg;
        } else if (lc->cmd == LC_SYMTAB) {
            symtab_cmd = (struct symtab_command *)lc;
        } else if (lc->cmd == LC_DYSYMTAB) {
            dysymtab_cmd = (struct dysymtab_command *)lc;
        }
        cur += lc->cmdsize;
    }
    if (!linkedit || !symtab_cmd || !dysymtab_cmd || !dysymtab_cmd->nindirectsyms)
        return;

    uintptr_t lbase = (uintptr_t)slide + linkedit->vmaddr - linkedit->fileoff;
    nlist_t *symtab = (nlist_t *)(lbase + symtab_cmd->symoff);
    char *strtab = (char *)(lbase + symtab_cmd->stroff);
    uint32_t *indirect = (uint32_t *)(lbase + dysymtab_cmd->indirectsymoff);

    cur = (uintptr_t)header + sizeof(mach_header_t);
    for (uint32_t i = 0; i < header->ncmds; i++) {
        struct load_command *lc = (struct load_command *)cur;
        if (lc->cmd != LC_SEGMENT_ARCH_DEPENDENT) {
            cur += lc->cmdsize;
            continue;
        }
        segment_command_t *seg = (segment_command_t *)lc;
        if (strcmp(seg->segname, SEG_DATA) != 0 &&
            strcmp(seg->segname, SEG_DATA_CONST) != 0) {
            cur += lc->cmdsize;
            continue;
        }
        section_t *sect = (section_t *)(seg + 1);
        for (uint32_t j = 0; j < seg->nsects; j++) {
            if ((sect[j].flags & SECTION_TYPE) == S_LAZY_SYMBOL_POINTERS ||
                (sect[j].flags & SECTION_TYPE) == S_NON_LAZY_SYMBOL_POINTERS) {
                perform_rebinding(&sect[j], slide, symtab, strtab, indirect);
            }
        }
        cur += lc->cmdsize;
    }
}

__attribute__((constructor))
static void tracker_init(void) {
    /* Resolve the real allocators before rebinding anything. */
    real_malloc  = dlsym(RTLD_DEFAULT, "malloc");
    real_free    = dlsym(RTLD_DEFAULT, "free");
    real_calloc  = dlsym(RTLD_DEFAULT, "calloc");
    real_realloc = dlsym(RTLD_DEFAULT, "realloc");

    const char *path = getenv("BAXAN_TRACKER_OUTPUT");
    if (path && path[0])
        tracker_fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    file_init = 1;

    /* Patch malloc/free/calloc/realloc pointers in every loaded image. */
    uint32_t c = _dyld_image_count();
    for (uint32_t i = 0; i < c; i++) {
        rebind_image(_dyld_get_image_header(i), _dyld_get_image_vmaddr_slide(i));
    }
}

/* ================================================================== */
/* Linux: LD_PRELOAD + dlsym(RTLD_NEXT)                                */
/* ================================================================== */
#else

void *malloc(size_t sz) {
    static void *(*rm)(size_t) = NULL;
    if (!rm) rm = dlsym(RTLD_NEXT, "malloc");
    void *p = rm(sz);
    if (p) {
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        snprintf(val,  sizeof(val),  "%zuB", sz);
        emit_declare(id, val, sz, addr);
    }
    return p;
}

void free(void *p) {
    static void (*rf)(void *) = NULL;
    if (!rf) rf = dlsym(RTLD_NEXT, "free");
    if (p) {
        char id[24], addr[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        emit_drop(id, addr);
    }
    rf(p);
}

void *calloc(size_t n, size_t s) {
    static void *(*rc)(size_t, size_t) = NULL;
    if (!rc) rc = dlsym(RTLD_NEXT, "calloc");
    void *p = rc(n, s);
    if (p) {
        size_t total = n * s;
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        snprintf(val,  sizeof(val),  "%zuB", total);
        emit_declare(id, val, total, addr);
    }
    return p;
}

void *realloc(void *p, size_t sz) {
    static void *(*rr)(void *, size_t) = NULL;
    if (!rr) rr = dlsym(RTLD_NEXT, "realloc");
    if (p) {
        char id[24], addr[24];
        snprintf(id,   sizeof(id),   "h_%p", p);
        snprintf(addr, sizeof(addr), "%p",   p);
        emit_drop(id, addr);
    }
    void *np = rr(p, sz);
    if (np) {
        char id[24], addr[24], val[24];
        snprintf(id,   sizeof(id),   "h_%p", np);
        snprintf(addr, sizeof(addr), "%p",   np);
        snprintf(val,  sizeof(val),  "%zuB", sz);
        emit_declare(id, val, sz, addr);
    }
    return np;
}

#endif /* __APPLE__ */
