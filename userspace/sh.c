#include <stdio.h>
#include <sys/syscall.h>

int main(int argc, char* argv[]) {
    printf("Starting shell process!\n");

    handle_t tty_dev = sys_open("device", "tty", 0);
    if (tty_dev < 0) {
        printf("sh: Unable to open tty device!\n");
        return -1;
    }

    int pid = sys_get_pid();

    if (sys_set_session_leader(-1) < 0) {
        printf("sh: Unable to create new session!\n");
        return -1;
    }

    if (sys_device_control(tty_dev, SET_CTTY, (void*)pid) < 0) {
        printf("sh: Unable to set controlling tty for this session!\n");
        return -1;
    }

    if (sys_device_control(tty_dev, SET_FOREGROUND_PGRP, (void*)pid) < 0) {
        printf("sh: Unable to set sh as foreground process!\n");
        return -1;
    }

    printf("sh: launching pipe demo\n");

    char *producer_args[] = {"/bin/producer"};
    handle_t producer_proc = sys_create_process(producer_args, 1, 0);
    if (producer_proc < 0) {
        printf("sh: failed to launch producer\n");
        return -1;
    }

    // Give producer time to create the named pipe before consumer tries to open it
    sys_delay_ms(150);

    char *consumer_args[] = {"/bin/consumer"};
    handle_t consumer_proc = sys_create_process(consumer_args, 1, 0);
    if (consumer_proc < 0) {
        printf("sh: failed to launch consumer\n");
        return -1;
    }

    sys_wait(producer_proc, -1);
    sys_wait(consumer_proc, -1);

    printf("sh: pipe demo finished\n");
    return 0;
}