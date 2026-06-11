// spawner — user program that spawns a child process (libhello.so)
// Depends on: libulib.so (ulib_hello, ulib_delay_100)
//
// Syscalls used:
//   0 = exit_process, 2 = write(ptr, len)
//   3 = delay(ms), 5 = create_process(path_ptr, path_len)

.text

.globl _start
.type _start, @function
_start:
    // Print greeting
    movq $2, %rax
    leaq .Lspawner_start(%rip), %rdi
    movq $.Lspawner_start_end - .Lspawner_start, %rsi
    syscall

    // Call shared lib
    callq ulib_hello@PLT

    // Spawn child process: syscall 5 (rdi = path ptr, rsi = path len)
    movq $5, %rax
    leaq .Lchild_path(%rip), %rdi
    movq $.Lchild_path_end - .Lchild_path, %rsi
    syscall

    // Print spawn result
    movq $2, %rax
    leaq .Lspawner_spawned(%rip), %rdi
    movq $.Lspawner_spawned_end - .Lspawner_spawned, %rsi
    syscall

    // Delay to let child run
    movq $3, %rax
    movq $2000, %rdi
    syscall

    // Call shared lib again after child runs
    callq ulib_delay_100@PLT

    // Print exit message
    movq $2, %rax
    leaq .Lspawner_done(%rip), %rdi
    movq $.Lspawner_done_end - .Lspawner_done, %rsi
    syscall

    // Exit process (syscall 0)
    movq $0, %rax
    syscall
    ud2
.size _start, . - _start


.section .rodata
.Lspawner_start:
    .ascii "spawner: starting"
.Lspawner_start_end:

.Lchild_path:
    .ascii "libhello.so"
.Lchild_path_end:

.Lspawner_spawned:
    .ascii "spawner: child process launched"
.Lspawner_spawned_end:

.Lspawner_done:
    .ascii "spawner: done"
.Lspawner_done_end:
