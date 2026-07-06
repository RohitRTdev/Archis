#include <sys/syscall.h>
#include <stdio.h>

int main(int argc, const char* argv[]) {
    handle_t tty = sys_open("device", "tty", OPEN_INHERITABLE_FLAG);
    if (tty < 0) {
        sys_exit(-1);
    }

    sys_duplicate_handle(-1, tty, STDIN_FILENO, TRUE);
    sys_duplicate_handle(-1, tty, STDOUT_FILENO, TRUE);
    sys_duplicate_handle(-1, tty, STDERR_FILENO, TRUE);
    stdio_init_std_handles();

    handle_t guard = sys_create_sync_object(SYNC_EVENT, 0, 0, 0, 0, "init.running");
    if (guard < 0) {
        printf("init: already running\n");
        return -1;
    }

    char* args[] = {"/bin/sh"};
    while (1) {
        process_info_t info;
        handle_t proc_handle = sys_create_process(args, 1, NULL, 0);
        sys_wait(proc_handle, -1);
        printf("init: Restarting shell process\n");
    }

    return 0;
}
