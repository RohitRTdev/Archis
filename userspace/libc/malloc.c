#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <pthread.h>
#include <sys/syscall.h>

#define ALIGN        16
#define PAGE_SZ      4096
#define CHUNK_SZ     (64 * 1024)
#define LARGE_THRESH (32 * 1024)
#define MIN_SPLIT    32   /* ALIGN + block header = 16 + 16 */

typedef struct block_hdr {
    size_t size_flags;       /* bit 0: free; remaining bits: data size */
    struct block_hdr *next;  /* next free block sorted by address */
} block_hdr_t;

typedef struct chunk {
    struct chunk *next;
    size_t size;  /* usable bytes after this header */
} chunk_t;

#define BLK_HDR_SZ  sizeof(block_hdr_t)   /* 16 */
#define CHUNK_HDR_SZ sizeof(chunk_t)       /* 16 */

#define BLK_SIZE(b)   ((b)->size_flags & ~(size_t)1)
#define BLK_FREE(b)   ((b)->size_flags &  (size_t)1)
#define BLK_DATA(b)   ((void *)((char *)(b) + BLK_HDR_SZ))
#define DATA_BLK(p)   ((block_hdr_t *)((char *)(p) - BLK_HDR_SZ))

static chunk_t     *g_chunks    = (void *)0;
static block_hdr_t *g_free_list = (void *)0;

static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;

static void lock_acquire(void) {
    pthread_mutex_lock(&g_lock);
}

static void lock_release(void) {
    pthread_mutex_unlock(&g_lock);
}

static size_t round_up(size_t n, size_t align) {
    return (n + align - 1) & ~(align - 1);
}

/* Insert block into free list, sorted by address. */
static void free_list_insert(block_hdr_t *b) {
    block_hdr_t **pp = &g_free_list;
    while (*pp && *pp < b) pp = &(*pp)->next;
    b->next = *pp;
    *pp = b;
}

/* Remove block from free list. Caller must know it's in the list. */
static void free_list_remove(block_hdr_t *b) {
    block_hdr_t **pp = &g_free_list;
    while (*pp && *pp != b) pp = &(*pp)->next;
    if (*pp) *pp = b->next;
}

/* Split block b so b holds `size` data bytes; remainder becomes a new free block. */
static void split_block(block_hdr_t *b, size_t size) {
    size_t old = BLK_SIZE(b);
    if (old - size < (size_t)MIN_SPLIT) return;
    block_hdr_t *rest = (block_hdr_t *)((char *)BLK_DATA(b) + size);
    rest->size_flags = (old - size - BLK_HDR_SZ) | 1;
    rest->next = (void *)0;
    free_list_insert(rest);
    b->size_flags = size;  /* allocated, so bit 0 = 0 */
}

/* Allocate a new chunk from the kernel and put its single free block in free list. */
static chunk_t *new_chunk(size_t min_data) {
    size_t total = CHUNK_HDR_SZ + BLK_HDR_SZ + min_data;
    if (total < (size_t)CHUNK_SZ) total = CHUNK_SZ;
    total = round_up(total, PAGE_SZ);

    void *ptr = (void *)0;
    if (sys_allocate_memory(total, &ptr) != E_SUCCESS || !ptr)
        return (void *)0;

    chunk_t *c = (chunk_t *)ptr;
    c->next = g_chunks;
    c->size = total - CHUNK_HDR_SZ;
    g_chunks = c;

    block_hdr_t *b = (block_hdr_t *)((char *)ptr + CHUNK_HDR_SZ);
    b->size_flags = (c->size - BLK_HDR_SZ) | 1;
    b->next = (void *)0;
    free_list_insert(b);

    return c;
}

void malloc_init(void) {
}

void *malloc(size_t size) {
    if (!size) return (void *)0;
    lock_acquire();

    size = round_up(size, ALIGN);

    block_hdr_t **pp = &g_free_list;
    while (*pp) {
        block_hdr_t *b = *pp;
        if (BLK_SIZE(b) >= size) {
            *pp = b->next;
            split_block(b, size);
            b->size_flags = BLK_SIZE(b);  /* clear free bit */
            lock_release();
            return BLK_DATA(b);
        }
        pp = &b->next;
    }

    if (!new_chunk(size)) { lock_release(); return (void *)0; }

    /* Retry — new chunk guarantees a block large enough. */
    pp = &g_free_list;
    while (*pp) {
        block_hdr_t *b = *pp;
        if (BLK_SIZE(b) >= size) {
            *pp = b->next;
            split_block(b, size);
            b->size_flags = BLK_SIZE(b);
            lock_release();
            return BLK_DATA(b);
        }
        pp = &b->next;
    }

    lock_release();
    return (void *)0;
}

void free(void *ptr) {
    if (!ptr) return;
    lock_acquire();

    block_hdr_t *b = DATA_BLK(ptr);
    b->size_flags |= 1;  /* mark free */
    b->next = (void *)0;
    free_list_insert(b);

    /* Coalesce forward. */
    char *end = (char *)BLK_DATA(b) + BLK_SIZE(b);
    if (b->next && (char *)b->next == end) {
        b->size_flags = (BLK_SIZE(b) + BLK_HDR_SZ + BLK_SIZE(b->next)) | 1;
        b->next = b->next->next;
    }

    /* Coalesce backward: find predecessor in sorted free list. */
    block_hdr_t *prev = (void *)0;
    block_hdr_t *cur = g_free_list;
    while (cur && cur != b) { prev = cur; cur = cur->next; }
    if (prev) {
        char *prev_end = (char *)BLK_DATA(prev) + BLK_SIZE(prev);
        if (prev_end == (char *)b) {
            prev->size_flags = (BLK_SIZE(prev) + BLK_HDR_SZ + BLK_SIZE(b)) | 1;
            prev->next = b->next;
        }
    }

    lock_release();
}

void *calloc(size_t nmemb, size_t size) {
    if (nmemb && size > (size_t)-1 / nmemb) return (void *)0;
    size_t total = nmemb * size;
    void *ptr = malloc(total);
    if (ptr) memset(ptr, 0, total);
    return ptr;
}

static chunk_t *find_chunk(block_hdr_t *b) {
    chunk_t *c = g_chunks;
    while (c) {
        char *start = (char *)c + CHUNK_HDR_SZ;
        char *end   = start + c->size;
        if ((char *)b >= start && (char *)b < end) return c;
        c = c->next;
    }
    return (void *)0;
}

void *realloc(void *ptr, size_t size) {
    if (!ptr) return malloc(size);
    if (!size) { free(ptr); return (void *)0; }

    lock_acquire();

    block_hdr_t *b = DATA_BLK(ptr);
    size_t old_size = BLK_SIZE(b);
    size_t new_size = round_up(size, ALIGN);

    if (old_size >= new_size) {
        /* Optionally split off excess. */
        if (old_size - new_size >= (size_t)MIN_SPLIT) {
            b->size_flags = new_size;
            split_block(b, new_size);
        }
        lock_release();
        return ptr;
    }

    /* Try to expand in-place by absorbing the next free block. */
    chunk_t *chunk = find_chunk(b);
    char *chunk_end = chunk ? (char *)chunk + CHUNK_HDR_SZ + chunk->size : (void *)0;
    block_hdr_t *next = (block_hdr_t *)((char *)BLK_DATA(b) + old_size);
    if (chunk_end && (char *)next + BLK_HDR_SZ <= chunk_end &&
            BLK_FREE(next) && old_size + BLK_HDR_SZ + BLK_SIZE(next) >= new_size) {
        free_list_remove(next);
        b->size_flags = old_size + BLK_HDR_SZ + BLK_SIZE(next);
        if (BLK_SIZE(b) - new_size >= (size_t)MIN_SPLIT)
            split_block(b, new_size);
        lock_release();
        return ptr;
    }

    lock_release();

    void *new_ptr = malloc(new_size);
    if (!new_ptr) return (void *)0;
    memcpy(new_ptr, ptr, old_size);
    free(ptr);
    return new_ptr;
}

void *aligned_alloc(size_t alignment, size_t size) {
    if (!alignment || (alignment & (alignment - 1))) return (void *)0;
    if (alignment <= ALIGN) return malloc(size);

    lock_acquire();

    size = round_up(size, ALIGN);

    block_hdr_t **pp = &g_free_list;
    while (*pp) {
        block_hdr_t *b = *pp;
        char *block_data = (char *)BLK_DATA(b);
        char *block_end  = block_data + BLK_SIZE(b);

        /* aligned_data must have room for its own header before it. */
        uintptr_t raw    = (uintptr_t)block_data + BLK_HDR_SZ;
        uintptr_t aligned = (raw + alignment - 1) & ~(uintptr_t)(alignment - 1);
        char *aligned_data = (char *)aligned;

        if (aligned_data + size > block_end) { pp = &b->next; continue; }

        /* Remove block from free list. */
        *pp = b->next;

        /* Leading fragment: from b's data start to aligned block header. */
        size_t lead = (size_t)((aligned_data - BLK_HDR_SZ) - block_data);
        if (lead >= (size_t)MIN_SPLIT) {
            b->size_flags = (lead - BLK_HDR_SZ) | 1;
            b->next = (void *)0;
            free_list_insert(b);
        }
        /* (If lead < MIN_SPLIT the bytes are wasted as internal padding.) */

        /* Set up aligned block. */
        block_hdr_t *ab = (block_hdr_t *)(aligned_data - BLK_HDR_SZ);
        size_t trail = (size_t)(block_end - aligned_data) - size;
        if (trail >= (size_t)MIN_SPLIT) {
            ab->size_flags = size;  /* allocated */
            block_hdr_t *tb = (block_hdr_t *)(aligned_data + size);
            tb->size_flags = (trail - BLK_HDR_SZ) | 1;
            tb->next = (void *)0;
            free_list_insert(tb);
        } else {
            ab->size_flags = (size_t)(block_end - aligned_data);
        }
        ab->next = (void *)0;

        lock_release();
        return aligned_data;
    }

    /* No suitable block found — get a chunk large enough. */
    if (!new_chunk(size + alignment)) { lock_release(); return (void *)0; }
    lock_release();

    /* Retry now that there's a large free block. */
    return aligned_alloc(alignment, size);
}
