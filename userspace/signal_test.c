#include <stdio.h>
#include <signal.h>
#include <unistd.h>

typedef struct {
    const char *signal_name;
    int id;
} signal_test_ctx_t;

void my_signal_handler_sig_int(void *ctx) {
    signal_test_ctx_t *c = (signal_test_ctx_t *)ctx;
    printf("signal_test: sigkill handler: name=%s id=%d\n", c->signal_name, c->id);
}

void my_signal_handler_sig_segv(void *ctx) {
    signal_test_ctx_t *c = (signal_test_ctx_t *)ctx;
    printf("signal_test: sigsegv handler: name=%s id=%d\n", c->signal_name, c->id);
}

void my_signal_handler_sig_ill(void *ctx) {
    signal_test_ctx_t *c = (signal_test_ctx_t *)ctx;
    printf("signal_test: sigill handler: name=%s id=%d\n", c->signal_name, c->id);
}

int main(void) {
    printf("signal_test: starting, registering handler for signals\n");

    signal_test_ctx_t int_ctx = { "SIGINT", SIGINT };
    signal_test_ctx_t segv_ctx = { "SIGSEGV", SIGSEGV };
    signal_test_ctx_t ill_ctx  = { "SIGILL",  SIGILL  };

    set_signal_handler(SIGINT, my_signal_handler_sig_int, &int_ctx);
    set_signal_handler(SIGSEGV, my_signal_handler_sig_segv, &segv_ctx);
    set_signal_handler(SIGILL,  my_signal_handler_sig_ill,  &ill_ctx);

    printf("signal_test: waiting for signal...\n");
    int remaining = sleep(10);
    printf("signal_test: delay completed with remaining %ds, exiting\n", remaining);
    return 0;
}
