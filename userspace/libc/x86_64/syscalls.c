#include <stdint.h>
#include <sys/syscall.h>

syscall_status_t do_syscall(uint64_t number, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6);


syscall_status_t sys_print(const char* msg) {
    return do_syscall(SYSCALL_PRINT, (uint64_t)msg, 0, 0, 0, 0, 0);
}

syscall_status_t sys_delay_ms(uint64_t ms) {
    return do_syscall(SYSCALL_DELAY_MS, ms, 0, 0, 0, 0, 0);
}

syscall_status_t sys_create_process(char *const args[], uint64_t len, uint64_t flags) {
    return do_syscall(SYSCALL_CREATE_PROCESS, (uint64_t)args, len, flags, 0, 0, 0);
}

//syscall_status_t sys_create_thread(const void *context) {
//    return do_syscall(SYSCALL_CREATE_THREAD, (uint64_t)context, 0, 0, 0, 0, 0);
//}

syscall_status_t sys_resume_process(uint64_t pid) {
    return do_syscall(SYSCALL_RESUME_PROCESS, pid, 0, 0, 0, 0, 0);
}

syscall_status_t sys_set_session_id(uint64_t pid, uint64_t sid) {
    return do_syscall(SYSCALL_RESUME_PROCESS, pid, sid, 0, 0, 0, 0);
}

syscall_status_t sys_get_pid() {
    return do_syscall(SYSCALL_GET_PID, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_get_process_info(uint64_t pid, process_info_t *const buf) {
    return do_syscall(SYSCALL_GET_PROCESS_INFO, pid, (uint64_t)buf, 0, 0, 0, 0);
}

syscall_status_t sys_close(uint64_t fd) {
    return do_syscall(SYSCALL_CLOSE, fd, 0, 0, 0, 0, 0);
}
