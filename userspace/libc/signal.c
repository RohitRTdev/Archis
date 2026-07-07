#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/syscall.h>
#include <pthread.h>

typedef struct {
    void (*handler)(void *);
    void *user_ctx;
} libc_sig_ctx_t;

extern void _libc_signal_start(void);

#define SIG_TABLE_SIZE 6

// Tracks the ctx currently registered with the kernel for each signal, so a
// re-registration can free the old one -- the kernel holds onto whatever
// pointer it was last given and reuses it for every future delivery, so
// nothing else ever frees it otherwise.
static libc_sig_ctx_t *g_sig_table[SIG_TABLE_SIZE];
static pthread_mutex_t g_sig_table_lock = PTHREAD_MUTEX_INITIALIZER;

void signal_init(void) {
    memset(g_sig_table, 0, sizeof(g_sig_table));
}

syscall_status_t set_signal_handler(uint8_t signal, void (*handler)(void *), void *user_ctx) {
    if (signal >= SIG_TABLE_SIZE)
        return E_INVALID;

    libc_sig_ctx_t *ctx = malloc(sizeof(libc_sig_ctx_t));
    if (!ctx)
        return E_OOM;
    ctx->handler = handler;
    ctx->user_ctx = user_ctx;

    pthread_mutex_lock(&g_sig_table_lock);
    libc_sig_ctx_t *old = g_sig_table[signal];
    g_sig_table[signal] = ctx;
    pthread_mutex_unlock(&g_sig_table_lock);

    syscall_status_t rc = sys_set_signal_handler(signal, (uint64_t)_libc_signal_start, ctx);
    if (rc != E_SUCCESS) {
        pthread_mutex_lock(&g_sig_table_lock);
        if (g_sig_table[signal] == ctx)
            g_sig_table[signal] = old;
        pthread_mutex_unlock(&g_sig_table_lock);
        free(ctx);
        return rc;
    }

    if (old)
        free(old);
    return E_SUCCESS;
}

void _libc_sig_handler(libc_sig_ctx_t *ctx) {
    ctx->handler(ctx->user_ctx);
    sys_sigreturn();
    while (1) {}
}
