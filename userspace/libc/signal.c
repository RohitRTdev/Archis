#include <stdint.h>
#include <stdlib.h>
#include <signal.h>
#include <sys/syscall.h>

typedef struct {
    void (*handler)(void *);
    void *user_ctx;
} libc_sig_ctx_t;

extern void _libc_signal_start(void);

syscall_status_t set_signal_handler(uint8_t signal, void (*handler)(void *), void *user_ctx) {
    libc_sig_ctx_t *ctx = malloc(sizeof(libc_sig_ctx_t));
    if (!ctx)
        return E_OOM;
    ctx->handler = handler;
    ctx->user_ctx = user_ctx;
    return sys_set_signal_handler(signal, (uint64_t)_libc_signal_start, ctx);
}

void _libc_sig_handler(libc_sig_ctx_t *ctx) {
    ctx->handler(ctx->user_ctx);
    free(ctx);
    sys_sigreturn();
    while (1) {}
}
