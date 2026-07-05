.global _start
.extern crt_main

.text

_start:
    // TOS is argc, followed by argc number of args, then envp
    movq (%rsp), %rdi
    leaq 8(%rsp), %rsi
    leaq 8(%rsi, %rdi, 8), %rdx
    call crt_main
    call exit

hang:
    jmp hang


exit:
    movslq %eax, %rdi
    movq $0, %rax
    syscall
    jmp hang   