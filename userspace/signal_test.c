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
    sys_exit(-2);
    sys_sigreturn();
    while(1) {}
}

int main(void) {
    printf("signal_test: starting, registering handler for SIGSEGV");
    syscall_status_t res = sys_set_signal_handler(SIGKILL, my_signal_handler_sig_kill, 0);
    syscall_status_t res1 = sys_set_signal_handler(SIGSEGV, my_signal_handler_sig_segv, 0);
    printf("signal_test: set_signal_handler returned %d", (int)res);

    printf("signal_test: waiting for signal...");
    printf("global address: %d", *(int*)0);
    sys_delay_ms(10000);

    printf("signal_test: delay completed, exiting");
    return 0;
}
