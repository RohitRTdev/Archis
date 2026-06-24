#pragma once

#include <stdint.h>

typedef int64_t handle_t;

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
    SYSCALL_SET_SESSION_LEADER,
    SYSCALL_GET_PID,
    SYSCALL_GET_PROCESS_INFO,
    SYSCALL_ALLOCATE_MEMORY,
    SYSCALL_DEALLOCATE_MEMORY,
    SYSCALL_SET_SIGNAL_HANDLER,
    SYSCALL_SIGRETURN,
    SYSCALL_CREATE_SYNC_OBJECT,
    SYSCALL_WAIT,
    SYSCALL_SIGNAL,
    SYSCALL_GET_TIME_MS,
    SYSCALL_DUPLICATE_HANDLE
};

typedef enum {
    SYNC_SEMAPHORE = 0,
    SYNC_EVENT     = 1
} sync_type_t;

typedef enum {
    E_SUCCESS = 0,
    E_INVALID = -1,
    E_OOM = -2,
    E_INTERNAL_FAILURE = -3,
    E_NOT_SUPPORTED = -4,
    E_DEV_STOPPED = -5,
    E_INVALID_MEMORY_RANGE = -6,
    E_PROCESS_TERMINATED = -7,
    E_NOPERM = -8,
    E_WAIT_INTERRUPTED = -11,
    E_TIMEOUT = -12
} syscall_status_t;

#define PROCESS_SUSPEND_FLAG ((uint64_t)1 << 0)

typedef enum {
    CLOCK_MONOTONIC = 0,
    CLOCK_WALL_TIME = 1
} clock_type_t;


typedef struct {
    uint64_t id;
    uint64_t pid;
    uint64_t sid;
} process_info_t;

syscall_status_t sys_exit(int64_t exit_code);
syscall_status_t sys_close(handle_t handle);
syscall_status_t sys_print(const char* msg);
syscall_status_t sys_delay_ms(size_t ms);
handle_t sys_create_process(char *const args[], size_t len, uint64_t flags);
syscall_status_t sys_create_thread(const void *context);
syscall_status_t sys_resume_process(uint64_t pid);
syscall_status_t sys_set_session_leader(uint64_t pid);
syscall_status_t sys_get_pid();
syscall_status_t sys_get_process_info(handle_t handle, process_info_t *const buf);
syscall_status_t sys_allocate_memory(size_t size, void **out);
syscall_status_t sys_deallocate_memory(void *addr, size_t size);
syscall_status_t sys_set_signal_handler(uint8_t signal, void (*handler)(void), void *user_ctx);
syscall_status_t sys_sigreturn(void);
syscall_status_t sys_create_sync_object(
    sync_type_t type,
    uint64_t init_count,
    uint64_t max_count,
    uint8_t auto_reset
);
syscall_status_t sys_wait(handle_t handle, ssize_t timeout);
syscall_status_t sys_signal(handle_t handle);
handle_t sys_duplicate_handle(handle_t target_proc, handle_t old, handle_t new, uint8_t is_inheritable);
syscall_status_t sys_get_time_ms(clock_type_t clock, uint64_t *out);

