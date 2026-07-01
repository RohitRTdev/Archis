#pragma once

#include <stdint.h>

typedef int64_t handle_t;

enum syscall_t {
    SYSCALL_EXIT_PROCESS,
    SYSCALL_EXIT_THREAD,
    SYSCALL_READ,
    SYSCALL_WRITE,
    SYSCALL_CLOSE,
    SYSCALL_OPEN,
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
    SYSCALL_DUPLICATE_HANDLE,
    SYSCALL_CREATE_PGRP,
    SYSCALL_GET_TID,
    SYSCALL_GET_THREAD_INFO,
    SYSCALL_DEVICE_CONTROL,
    SYSCALL_SEEK,
    SYSCALL_FSTAT,
    SYSCALL_READDIR,
    SYSCALL_DELETE_FILE,
    SYSCALL_RENAME_FILE,
    SYSCALL_MKDIR,
    SYSCALL_RMDIR,
    SYSCALL_CREATE_FILE,
    SYSCALL_CREATE_SYMLINK,
    SYSCALL_READLINK,
    SYSCALL_CREATE_PIPE
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
    E_TIMEOUT          = -12,
    E_BUF_TOO_SMALL    = -20,
    E_NO_DIR_ENTRIES   = -21
} syscall_status_t;

#define PROCESS_SUSPEND_FLAG    ((uint64_t)1 << 0)
#define OPEN_INHERITABLE_FLAG   ((uint64_t)1 << 0)
#define CREATE_FILE_EXIST_FLAG  ((uint64_t)1 << 1)
#define OPEN_WRITE_FLAG         ((uint64_t)1 << 2)

typedef enum {
    CLOCK_MONOTONIC = 0,
    CLOCK_WALL_TIME = 1
} clock_type_t;

#define SET_FOREGROUND_PGRP (8)
#define SET_CTTY (9)

typedef struct {
    uint64_t size;
    uint16_t mode;
} file_stat_t;

#define FILE_MODE_FILE    (1 << 0)
#define FILE_MODE_DIR     (1 << 1)
#define FILE_MODE_SYMLINK (1 << 2)

typedef enum {
    SEEK_SET = 0,
    SEEK_CUR = 1,
    SEEK_END = 2
} seek_whence_t;


typedef struct {
    uint64_t id;
    uint64_t pid;
    uint64_t sid;
    int64_t  exit_code;
} process_info_t;

typedef struct {
    uint64_t id;
    int64_t  exit_code;
} thread_info_t;

syscall_status_t sys_exit(ssize_t exit_code);
syscall_status_t sys_close(handle_t handle);
handle_t         sys_open(const char *type, const char *name, uint64_t flags);
syscall_status_t sys_read(handle_t handle, void *buf, size_t len, size_t *bytes_read);
syscall_status_t sys_write(handle_t handle, const void *buf, size_t len, size_t *bytes_written);
syscall_status_t sys_delay_ms(size_t ms);
handle_t         sys_create_process(char *const args[], size_t len, uint64_t flags);
syscall_status_t sys_create_thread(uint64_t fn_addr, void *context);
syscall_status_t sys_exit_thread(void);
syscall_status_t sys_resume_process(handle_t process_handle);
syscall_status_t sys_set_session_leader(handle_t process_handle);
syscall_status_t sys_get_pid();
syscall_status_t sys_get_process_info(handle_t handle, process_info_t *const buf);
syscall_status_t sys_allocate_memory(size_t size, void **out);
syscall_status_t sys_deallocate_memory(void *addr, size_t size);
syscall_status_t sys_set_signal_handler(uint8_t signal, uint64_t handler_addr, void *user_ctx);
syscall_status_t sys_sigreturn(void);
handle_t sys_create_sync_object(
    sync_type_t type,
    uint64_t init_count,
    uint64_t max_count,
    uint8_t auto_reset,
    boolean_t is_inheritable,
    const char *name
);
syscall_status_t sys_wait(handle_t handle, ssize_t timeout);
syscall_status_t sys_signal(handle_t handle);
handle_t         sys_duplicate_handle(handle_t target_proc, handle_t old, handle_t new, boolean_t is_inheritable);
syscall_status_t sys_create_pgrp(handle_t process_handle);
syscall_status_t sys_get_time_ms(clock_type_t clock, size_t *out);
uint64_t         sys_get_tid(void);
syscall_status_t sys_get_thread_info(handle_t handle, thread_info_t *out);
syscall_status_t sys_device_control(handle_t handle, size_t minor_code, void* command);
handle_t         sys_create_file(const char *path, uint64_t flags);
syscall_status_t sys_create_symlink(const char *path, const char *target);
syscall_status_t sys_delete_file(const char *path);
syscall_status_t sys_rename_file(const char *from, const char *to);
syscall_status_t sys_mkdir(const char *path);
syscall_status_t sys_rmdir(const char *path);
ssize_t          sys_seek(handle_t handle, ssize_t offset, seek_whence_t whence);
syscall_status_t sys_fstat(handle_t handle, file_stat_t *out);
syscall_status_t sys_readdir(handle_t handle, size_t offset, char *buf, size_t buf_len, size_t *bytes_written);
syscall_status_t sys_readlink(const char *path, char *buf, size_t buf_len, size_t *bytes_written);
syscall_status_t sys_create_pipe(handle_t *read_handle, handle_t *write_handle, const char *name, boolean_t is_inheritable);
