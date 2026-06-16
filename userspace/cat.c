#include <common.h>
#include <sys/syscall.h>
#include <stdio.h>

int main(void) {
    printf("Hello from cat");

    char *const args[] = {"tester", "arg1", "arg2"};
    int64_t proc_fd = sys_create_process(args, 3, PROCESS_SUSPEND_FLAG);  
    if (proc_fd < 0) {
        printf("Process creation failed! Error code: %ld", proc_fd);
    }
    
    process_info_t info = {};
    if (sys_get_process_info(proc_fd, &info) < 0) {
        printf("Failed to get process info!");
        return -1;
    }

    printf("pid: %lu, ppid: %lu, sid: %lu", info.id, info.pid, info.sid);

    if (sys_set_session_leader(info.id) < 0) {
        printf("Failed to set session id!");
        return -1;
    }

    if (sys_get_process_info(proc_fd, &info) < 0) {
        printf("Failed to get process info!");
        return -1;
    }

    printf("New -> pid: %lu, ppid: %lu, sid: %lu", info.id, info.pid, info.sid);
    if (sys_resume_process(info.id) < 0) {
        printf("Process id: %lu could not be resumed", info.id);
        return -1;
    }

    printf("Exiting cat");
    return common_add(1,2);
}
