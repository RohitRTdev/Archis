#pragma once

#include <stdint.h>

enum Syscall {
    SYSCALL_EXIT_PROCESS,
    SYSCALL_EXIT_THREAD,
    SYSCALL_PRINT,
    SYSCALL_DELAY_MS,
    SYSCALL_CREATE_THREAD,
    SYSCALL_CREATE_PROCESS
};


void sys_print(const char* msg, uint64_t len);
void sys_delay_ms(uint64_t ms);
