#include <stdint.h>
#include <sys/syscall.h>

void do_syscall(uint64_t number, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6);


void sys_print(const char* msg, uint64_t len) {
    do_syscall(SYSCALL_PRINT, (uint64_t)msg, len, 0, 0, 0, 0);
}

void sys_delay_ms(uint64_t ms) {
    do_syscall(SYSCALL_DELAY_MS, ms, 0, 0, 0, 0, 0);
}