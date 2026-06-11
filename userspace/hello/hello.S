// hello — user program that exercises ulib, threads, and syscalls
// Depends on: libulib.so (ulib_hello, ulib_delay_100, ulib_get_magic)
//
// Syscalls used:
//   0 = exit_process, 1 = exit_thread, 2 = write(ptr, len)
//   3 = delay(ms), 4 = create_thread(fn_ptr, ctx_ptr)

.text

.globl _start
.type _start, @function
_start:
    // Print greeting
    movq $2, %rax
    leaq .Lhello_start_msg(%rip), %rdi
    movq $.Lhello_start_msg_end - .Lhello_start_msg, %rsi
    syscall

    // Call ulib_hello — cross-module dependency call
    callq ulib_hello@PLT

    // Call ulib_get_magic and verify the return value
    callq ulib_get_magic@PLT
    cmpq $0xCAFE, %rax
    jne .Lmagic_fail

    // Print magic-ok message
    movq $2, %rax
    leaq .Lhello_magic_ok(%rip), %rdi
    movq $.Lhello_magic_ok_end - .Lhello_magic_ok, %rsi
    syscall
    jmp .Lmagic_done

.Lmagic_fail:
    movq $2, %rax
    leaq .Lhello_magic_bad(%rip), %rdi
    movq $.Lhello_magic_bad_end - .Lhello_magic_bad, %rsi
    syscall

.Lmagic_done:
    // Call ulib_delay_100
    callq ulib_delay_100@PLT

    // Create a user thread (syscall 4: rdi = fn ptr, rsi = context)
    movq $4, %rax
    leaq hello_thread_fn(%rip), %rdi
    xorq %rsi, %rsi
    syscall

    // Delay 500ms so the thread can run
    movq $3, %rax
    movq $500, %rdi
    syscall

    // Print exit message
    movq $2, %rax
    leaq .Lhello_exit_msg(%rip), %rdi
    movq $.Lhello_exit_msg_end - .Lhello_exit_msg, %rsi
    syscall

    // Exit process (syscall 0)
    movq $0, %rax
    syscall
    ud2
.size _start, . - _start


// Thread entry — called by kernel via sysret; context pointer at [rsp]
.globl hello_thread_fn
.hidden hello_thread_fn
.type hello_thread_fn, @function
hello_thread_fn:
    // Print thread greeting
    movq $2, %rax
    leaq .Lthread_msg(%rip), %rdi
    movq $.Lthread_msg_end - .Lthread_msg, %rsi
    syscall

    // Call shared lib from thread context
    callq ulib_hello@PLT

    // Short delay
    movq $3, %rax
    movq $200, %rdi
    syscall

    // Print thread exit
    movq $2, %rax
    leaq .Lthread_exit_msg(%rip), %rdi
    movq $.Lthread_exit_msg_end - .Lthread_exit_msg, %rsi
    syscall

    // Exit thread (syscall 1)
    movq $1, %rax
    syscall
    ud2
.size hello_thread_fn, . - hello_thread_fn


.section .rodata
.Lhello_start_msg:
    .ascii "hello: starting"
.Lhello_start_msg_end:

.Lhello_magic_ok:
    .ascii "hello: ulib_get_magic returned 0xCAFE — linking OK"
.Lhello_magic_ok_end:

.Lhello_magic_bad:
    .ascii "hello: ulib_get_magic MISMATCH"
.Lhello_magic_bad_end:

.Lhello_exit_msg:
    .ascii "hello: exiting"
.Lhello_exit_msg_end:

.Lthread_msg:
    .ascii "hello: thread running"
.Lthread_msg_end:

.Lthread_exit_msg:
    .ascii "hello: thread exiting"
.Lthread_exit_msg_end:
