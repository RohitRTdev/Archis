#include <stdint.h>
#include <sys/syscall.h>

syscall_status_t do_syscall(uint64_t number, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6);

syscall_status_t sys_exit(ssize_t exit_code) {
    return do_syscall(SYSCALL_EXIT_PROCESS, (uint64_t)exit_code, 0, 0, 0, 0, 0);
}

handle_t sys_open(const char *type, const char *name, uint64_t flags) {
    return do_syscall(SYSCALL_OPEN, (uint64_t)type, (uint64_t)name, flags, 0, 0, 0);
}

syscall_status_t sys_read(handle_t handle, void *buf, size_t len, size_t *bytes_read) {
    return do_syscall(SYSCALL_READ, (uint64_t)handle, (uint64_t)buf, (uint64_t)len, (uint64_t)bytes_read, 0, 0);
}

syscall_status_t sys_write(handle_t handle, const void *buf, size_t len, size_t *bytes_written) {
    return do_syscall(SYSCALL_WRITE, (uint64_t)handle, (uint64_t)buf, (uint64_t)len, (uint64_t)bytes_written, 0, 0);
}

syscall_status_t sys_delay_ms(size_t ms) {
    return do_syscall(SYSCALL_DELAY_MS, ms, 0, 0, 0, 0, 0);
}

handle_t sys_create_process(char *const args[], size_t len, char *const envp[], uint64_t flags) {
    return do_syscall(SYSCALL_CREATE_PROCESS, (uint64_t)args, len, (uint64_t)envp, flags, 0, 0);
}

syscall_status_t sys_create_thread(uint64_t fn_addr, void *context) {
    return do_syscall(SYSCALL_CREATE_THREAD, fn_addr, (uint64_t)context, 0, 0, 0, 0);
}

syscall_status_t sys_exit_thread(void) {
    return do_syscall(SYSCALL_EXIT_THREAD, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_resume_process(handle_t process_handle) {
    return do_syscall(SYSCALL_RESUME_PROCESS, process_handle, 0, 0, 0, 0, 0);
}

syscall_status_t sys_set_session_leader(handle_t process_handle) {
    return do_syscall(SYSCALL_SET_SESSION_LEADER, process_handle, 0, 0, 0, 0, 0);
}

syscall_status_t sys_get_pid() {
    return do_syscall(SYSCALL_GET_PID, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_get_process_info(handle_t handle, process_info_t *const buf) {
    return do_syscall(SYSCALL_GET_PROCESS_INFO, (uint64_t)handle, (uint64_t)buf, 0, 0, 0, 0);
}

syscall_status_t sys_close(handle_t handle) {
    return do_syscall(SYSCALL_CLOSE, (uint64_t)handle, 0, 0, 0, 0, 0);
}

syscall_status_t sys_allocate_memory(size_t size, void **out) {
    return do_syscall(SYSCALL_ALLOCATE_MEMORY, size, (uint64_t)out, 0, 0, 0, 0);
}

syscall_status_t sys_deallocate_memory(void *addr, size_t size) {
    return do_syscall(SYSCALL_DEALLOCATE_MEMORY, (uint64_t)addr, size, 0, 0, 0, 0);
}

syscall_status_t sys_set_signal_handler(uint8_t signal, uint64_t handler_addr, void *user_ctx) {
    return do_syscall(SYSCALL_SET_SIGNAL_HANDLER, signal, handler_addr, (uint64_t)user_ctx, 0, 0, 0);
}

syscall_status_t sys_sigreturn(void) {
    return do_syscall(SYSCALL_SIGRETURN, 0, 0, 0, 0, 0, 0);
}

handle_t sys_create_sync_object(
    sync_type_t type,
    uint64_t init_count,
    uint64_t max_count,
    uint8_t auto_reset,
    boolean_t is_inheritable,
    const char *name
) {
    return do_syscall(SYSCALL_CREATE_SYNC_OBJECT, (uint64_t)type, init_count, max_count, auto_reset, (uint64_t)is_inheritable, (uint64_t)name);
}

syscall_status_t sys_wait(handle_t handle, ssize_t timeout) {
    return do_syscall(SYSCALL_WAIT, (uint64_t)handle, timeout, 0, 0, 0, 0);
}

syscall_status_t sys_signal(handle_t handle) {
    return do_syscall(SYSCALL_SIGNAL, (uint64_t)handle, 0, 0, 0, 0, 0);
}

handle_t sys_duplicate_handle(handle_t target_proc, handle_t old, handle_t new, boolean_t is_inheritable) {
    return do_syscall(SYSCALL_DUPLICATE_HANDLE, (uint64_t)target_proc, (uint64_t)old, (uint64_t)new, (uint64_t)is_inheritable, 0, 0);
}

syscall_status_t sys_set_pgrp(handle_t process_handle, size_t target_pgid) {
    return do_syscall(SYSCALL_SET_PGRP, (uint64_t)process_handle, target_pgid, 0, 0, 0, 0);
}

syscall_status_t sys_get_time_ms(clock_type_t clock, size_t *out) {
    return do_syscall(SYSCALL_GET_TIME_MS, (uint64_t)clock, (uint64_t)out, 0, 0, 0, 0);
}

uint64_t sys_get_tid(void) {
    return (uint64_t)do_syscall(SYSCALL_GET_TID, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_get_thread_info(handle_t handle, thread_info_t *out) {
    return do_syscall(SYSCALL_GET_THREAD_INFO, (uint64_t)handle, (uint64_t)out, 0, 0, 0, 0);
}

syscall_status_t sys_device_control(handle_t handle, size_t minor_code, void* command) {
    return do_syscall(SYSCALL_DEVICE_CONTROL, (uint64_t)handle, minor_code, (uint64_t)command, 0, 0, 0);
}

handle_t sys_create_file(const char *path, uint64_t flags) {
    return do_syscall(SYSCALL_CREATE_FILE, (uint64_t)path, flags, 0, 0, 0, 0);
}

syscall_status_t sys_create_symlink(const char *path, const char *target) {
    return do_syscall(SYSCALL_CREATE_SYMLINK, (uint64_t)path, (uint64_t)target, 0, 0, 0, 0);
}

syscall_status_t sys_delete(const char *path) {
    return do_syscall(SYSCALL_DELETE, (uint64_t)path, 0, 0, 0, 0, 0);
}

syscall_status_t sys_rename_file(const char *from, const char *to) {
    return do_syscall(SYSCALL_RENAME_FILE, (uint64_t)from, (uint64_t)to, 0, 0, 0, 0);
}

syscall_status_t sys_mkdir(const char *path) {
    return do_syscall(SYSCALL_MKDIR, (uint64_t)path, 0, 0, 0, 0, 0);
}

syscall_status_t sys_mount(const char *device, const char *path) {
    return do_syscall(SYSCALL_MOUNT, (uint64_t)device, (uint64_t)path, 0, 0, 0, 0);
}

syscall_status_t sys_unmount(const char *path) {
    return do_syscall(SYSCALL_UNMOUNT, (uint64_t)path, 0, 0, 0, 0, 0);
}

syscall_status_t sys_stat(const char *path, uint64_t flags, file_stat_t *out) {
    return do_syscall(SYSCALL_STAT, (uint64_t)path, flags, (uint64_t)out, 0, 0, 0);
}

ssize_t sys_seek(handle_t handle, ssize_t offset, seek_whence_t whence) {
    return do_syscall(SYSCALL_SEEK, (uint64_t)handle, (uint64_t)offset, (uint64_t)whence, 0, 0, 0);
}

syscall_status_t sys_fstat(handle_t handle, file_stat_t *out) {
    return do_syscall(SYSCALL_FSTAT, (uint64_t)handle, (uint64_t)out, 0, 0, 0, 0);
}

syscall_status_t sys_readdir(handle_t handle, size_t offset, char *buf, size_t buf_len, size_t *bytes_written) {
    return do_syscall(SYSCALL_READDIR, (uint64_t)handle, offset, (uint64_t)buf, buf_len, (uint64_t)bytes_written, 0);
}

syscall_status_t sys_readlink(const char *path, char *buf, size_t buf_len, size_t *bytes_written) {
    return do_syscall(SYSCALL_READLINK, (uint64_t)path, (uint64_t)buf, buf_len, (uint64_t)bytes_written, 0, 0);
}

syscall_status_t sys_create_pipe(handle_t *read_handle, handle_t *write_handle, const char *name, boolean_t is_inheritable) {
    return do_syscall(SYSCALL_CREATE_PIPE, (uint64_t)read_handle, (uint64_t)write_handle, (uint64_t)name, (uint64_t)is_inheritable, 0, 0);
}

syscall_status_t sys_chdir(const char *path) {
    return do_syscall(SYSCALL_CHDIR, (uint64_t)path, 0, 0, 0, 0, 0);
}

syscall_status_t sys_getcwd(char *buf, size_t buf_len, size_t *bytes_written) {
    return do_syscall(SYSCALL_GETCWD, (uint64_t)buf, buf_len, (uint64_t)bytes_written, 0, 0, 0);
}

syscall_status_t sys_issue_signal(int64_t target, uint8_t signal) {
    return do_syscall(SYSCALL_ISSUE_SIGNAL, (uint64_t)target, signal, 0, 0, 0, 0);
}

syscall_status_t sys_shutdown(boolean_t restart) {
    return do_syscall(SYSCALL_SHUTDOWN, restart, 0, 0, 0, 0, 0);
}

syscall_status_t sys_intf_request(const char *intf_name, void *request) {
    return do_syscall(SYSCALL_INTF_REQUEST, (uint64_t)intf_name, (uint64_t)request, 0, 0, 0, 0);
}

syscall_status_t sys_terminate_process(int64_t pid, int64_t exit_code) {
    return do_syscall(SYSCALL_TERMINATE_PROCESS, (uint64_t)pid, (uint64_t)exit_code, 0, 0, 0, 0);
}

syscall_status_t sys_terminate_thread(int64_t tid, int64_t exit_code) {
    return do_syscall(SYSCALL_TERMINATE_THREAD, (uint64_t)tid, (uint64_t)exit_code, 0, 0, 0, 0);
}
