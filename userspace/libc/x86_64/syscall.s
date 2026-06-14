
// Only 6 args are allowed at max
// rax - syscall number
// rdi, rsi, rdx, r10, r8, r9
// This follows the x86_64 sysv syscall abi convention
.global do_syscall
do_syscall:
    movq %rdi, %rax
    movq %rsi, %rdi
    movq %rdx, %rsi
    movq %rcx, %rdx
    movq %r8, %r10
    movq %r9, %r8
    movq 8(%rsp), %r9
    syscall 
    ret