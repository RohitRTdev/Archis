#include <stdint.h>
#include <sys/syscall.h>

syscall_status_t do_syscall(uint64_t number, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6);


syscall_status_t sys_print(const char* msg) {
    return do_syscall(SYSCALL_PRINT, (uint64_t)msg, 0, 0, 0, 0, 0);
}

syscall_status_t sys_delay_ms(uint64_t ms) {
    return do_syscall(SYSCALL_DELAY_MS, ms, 0, 0, 0, 0, 0);
}

syscall_status_t sys_create_process(char *const args[], uint64_t len) {
    return do_syscall(SYSCALL_CREATE_PROCESS, (uint64_t)args, len, 0, 0, 0, 0);
}
