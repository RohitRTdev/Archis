
// Signal and thread trampoline stubs.
// Both entry points are jumped to by the kernel (no return address on stack).
// RSP is 16-byte aligned at entry; user_ctx sits at [rsp - 8].

.global _libc_signal_start
_libc_signal_start:
    movq -8(%rsp), %rdi
    call _libc_sig_handler

.global _libc_pthread_thread_start
_libc_pthread_thread_start:
    movq -8(%rsp), %rdi
    call _libc_pthread_thread_handler
