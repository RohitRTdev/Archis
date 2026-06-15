#pragma once

#include <stdint.h>

enum syscall_t {
    SYSCALL_EXIT_PROCESS,
    SYSCALL_EXIT_THREAD,
    SYSCALL_READ,
    SYSCALL_PRINT,
    SYSCALL_CLOSE,
    SYSCALL_OPEN_FILE,
    SYSCALL_OPEN_DEVICE,
    SYSCALL_DELAY_MS,
    SYSCALL_CREATE_THREAD,
    SYSCALL_CREATE_PROCESS,
    SYSCALL_RESUME_PROCESS,
    SYSCALL_SET_SESSION_ID,
    SYSCALL_GET_PID,
    SYSCALL_GET_PROCESS_INFO
};

typedef enum {
    E_SUCCESS = 0,
    E_INVALID = -1,
    E_OOM = -2,
    E_INTERNAL_FAILURE = -3,
    E_NOT_SUPPORTED = -4,
    E_DEV_STOPPED = -5,
    E_INVALID_MEMORY_RANGE = -6,
    E_PROCESS_TERMINATED = -7,
    E_NOPERM = -8
} syscall_status_t;

const uint64_t PROCESS_SUSPEND_FLAG = 1 << 0;


typedef struct {
    uint64_t id;
    uint64_t pid;
    uint64_t sid;
} process_info_t;

syscall_status_t sys_close(uint64_t fd);
syscall_status_t sys_print(const char* msg);
syscall_status_t sys_delay_ms(uint64_t ms);
syscall_status_t sys_create_process(char *const args[], uint64_t len, uint64_t flags);
syscall_status_t sys_create_thread(const void *context);
syscall_status_t sys_resume_process(uint64_t pid);
syscall_status_t sys_set_session_id(uint64_t pid, uint64_t sid);
syscall_status_t sys_get_pid();
syscall_status_t sys_get_process_info(uint64_t pid, process_info_t *const buf);

