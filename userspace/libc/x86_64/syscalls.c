#include <stdint.h>
#include <sys/syscall.h>

syscall_status_t do_syscall(uint64_t number, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6);

syscall_status_t sys_exit(int64_t exit_code) {
    return do_syscall(SYSCALL_EXIT_PROCESS, (uint64_t)exit_code, 0, 0, 0, 0, 0);
}

syscall_status_t sys_print(const char* msg) {
    return do_syscall(SYSCALL_PRINT, (uint64_t)msg, 0, 0, 0, 0, 0);
}

syscall_status_t sys_delay_ms(size_t ms) {
    return do_syscall(SYSCALL_DELAY_MS, ms, 0, 0, 0, 0, 0);
}

handle_t sys_create_process(char *const args[], size_t len, uint64_t flags) {
    return do_syscall(SYSCALL_CREATE_PROCESS, (uint64_t)args, len, flags, 0, 0, 0);
}

syscall_status_t sys_create_thread(uint64_t fn_addr, void *context) {
    return do_syscall(SYSCALL_CREATE_THREAD, fn_addr, (uint64_t)context, 0, 0, 0, 0);
}

syscall_status_t sys_exit_thread(void) {
    return do_syscall(SYSCALL_EXIT_THREAD, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_resume_process(uint64_t pid) {
    return do_syscall(SYSCALL_RESUME_PROCESS, pid, 0, 0, 0, 0, 0);
}

syscall_status_t sys_set_session_leader(uint64_t pid) {
    return do_syscall(SYSCALL_SET_SESSION_LEADER, pid, 0, 0, 0, 0, 0);
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

syscall_status_t sys_create_sync_object(
    sync_type_t type, 
    uint64_t init_count, 
    uint64_t max_count,
    uint8_t auto_reset
) {
    return do_syscall(SYSCALL_CREATE_SYNC_OBJECT, (uint64_t)type, init_count, max_count, auto_reset, 0, 0);
}

syscall_status_t sys_wait(handle_t handle, ssize_t timeout) {
    return do_syscall(SYSCALL_WAIT, (uint64_t)handle, timeout, 0, 0, 0, 0);
}

syscall_status_t sys_signal(handle_t handle) {
    return do_syscall(SYSCALL_SIGNAL, (uint64_t)handle, 0, 0, 0, 0, 0);
}

handle_t sys_duplicate_handle(handle_t target_proc, handle_t old, handle_t new, uint8_t is_inheritable) {
    return do_syscall(SYSCALL_DUPLICATE_HANDLE, (uint64_t)target_proc, (uint64_t)old, (uint64_t)new, (uint64_t)is_inheritable, 0, 0);
}

syscall_status_t sys_get_time_ms(clock_type_t clock, uint64_t *out) {
    return do_syscall(SYSCALL_GET_TIME_MS, (uint64_t)clock, (uint64_t)out, 0, 0, 0, 0);
}

uint64_t sys_get_tid(void) {
    return (uint64_t)do_syscall(SYSCALL_GET_TID, 0, 0, 0, 0, 0, 0);
}

syscall_status_t sys_get_thread_info(handle_t handle, thread_info_t *out) {
    return do_syscall(SYSCALL_GET_THREAD_INFO, (uint64_t)handle, (uint64_t)out, 0, 0, 0, 0);
}
