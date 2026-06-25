#include <stdio.h>
#include <sys/syscall.h>
#include <signal.h>

volatile boolean_t IS_SIGNALLED = FALSE;

void sigint_handler(void* ctx) {
    (void)ctx;
    IS_SIGNALLED = TRUE;
}


int main(int argc, char* argv[]) {
    printf("Starting shell process!\n");

    handle_t tty_dev = sys_open_device("tty", 0);
    if (tty_dev < 0) {
        printf("sh: Unable to open tty device!");
        return -1;
    }

    int pid = sys_get_pid();
    printf("Opened tty handle: %d by pid: %d", tty_dev, pid);

    if (sys_set_session_leader(-1) < 0) {
        printf("sh: Unable to create new session!");
        return -1;
    }

    if (sys_device_control(tty_dev, SET_CTTY, (void*)1) < 0) {
        printf("sh: Unable to set controlling tty for this session!");
        return -1;
    } 

    if (sys_device_control(tty_dev, SET_FOREGROUND_PGRP, (void*)pid) < 0) {
        printf("sh: Unable to set sh as foreground process!");
        return -1;
    } 

    if (set_signal_handler(SIGINT, sigint_handler, NULL) < 0) {
        printf("sh: Unable to set signal handler!");
        return -1;
    }

    while(1) {
        IS_SIGNALLED = FALSE;
        printf(">");

        // Todo: Wait for input
        while(!IS_SIGNALLED) {}
        printf("\n");
    }

    return 0;
}