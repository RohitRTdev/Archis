#pragma once

#include <stdint.h>

enum syscall_t {
    SYSCALL_EXIT_PROCESS,
    SYSCALL_EXIT_THREAD,
    SYSCALL_READ,
    SYSCALL_PRINT,
    SYSCALL_OPEN_FILE,
    SYSCALL_OPEN_DEVICE,
    SYSCALL_DELAY_MS,
    SYSCALL_CREATE_THREAD,
    SYSCALL_CREATE_PROCESS
};

typedef enum {
    E_SUCCESS = 0,
    E_INVALID = -1,
    E_OOM = -2,
    E_INTERNAL_FAILURE = -3,
    E_NOT_SUPPORTED = -4,
    E_DEV_STOPPED = -5,
    E_INVALID_MEMORY_RANGE = -6
} syscall_status_t;


syscall_status_t sys_print(const char* msg);
syscall_status_t sys_delay_ms(uint64_t ms);
syscall_status_t sys_create_process(char *const args[], uint64_t len);
