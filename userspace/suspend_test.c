#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char* argv[]) {
    if (argc == 2 && !strcmp(argv[1], "--no-exec")) {
        return 0;
    }

    char* args[] = {"/bin/suspend_test", "--no-exec"};
    handle_t handle = sys_create_process(args, 2, environ, PROCESS_SUSPEND_FLAG);
    if (handle < 0) {
        fprintf(stderr, "suspend_test: Failed to create child process\n");
        return -1;
    }

    sleep(20);
    sys_resume_process(handle);
    return 0;
}