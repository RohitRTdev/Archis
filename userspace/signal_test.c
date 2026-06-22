#include <sys/syscall.h>
#include <stdio.h>
#include <signal.h>

void my_signal_handler_sig_kill(void) {
    printf("signal_test: handler invoked for sigkill! calling sigreturn");
    sys_delay_ms(10000);
    sys_sigreturn();
    while(1) {}
}

void my_signal_handler_sig_segv(void) {
    printf("signal_test: handler invoked for sigsegv! calling sigreturn");
    sys_sigreturn();
    while(1) {}
}

void my_signal_handler_sig_ill(void) {
    printf("signal_test: handler invoked for sigill! calling sigreturn");
    sys_sigreturn();
    while(1) {}
}

int main(void) {
    printf("signal_test: starting, registering handler for signals");
    sys_set_signal_handler(SIGKILL, my_signal_handler_sig_kill, 0);
    sys_set_signal_handler(SIGSEGV, my_signal_handler_sig_segv, 0);
    sys_set_signal_handler(SIGILL, my_signal_handler_sig_ill, 0);

    printf("signal_test: waiting for signal...");
    int res = sys_delay_ms(10000);
    printf("signal_test: delay completed with res %d, exiting", res);
    return 0;
}
