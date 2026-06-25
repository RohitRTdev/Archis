#include <sys/syscall.h>
#include <stdio.h>

int main(int argc, const char* argv[]) {
    handle_t tty = sys_open_device("tty", OPEN_INHERITABLE_FLAG);
    if (tty < 0) {
        sys_exit(-1);
    }

    sys_duplicate_handle(-1, tty, STDOUT_FILENO, TRUE);
    sys_duplicate_handle(-1, tty, STDOUT_FILENO, TRUE);

    printf("Hello from init\n");

    char* args[] = {"/bin/sh"};
    process_info_t info;
    handle_t proc_handle = sys_create_process(args, 1, 0);
    sys_get_process_info(proc_handle, &info);
    sys_wait(proc_handle, -1);
    printf("Shell process killed!\n");

    return 0;
}
