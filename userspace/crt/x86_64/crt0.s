.global _start
.extern main

.text

_start:
    // TOS is argc, followed by argc number of args
    movq (%rsp), %rdi
    leaq 8(%rsp), %rsi 
    call main
    call exit

hang:
    jmp hang


exit:
    movslq %eax, %rdi
    movq $0, %rax
    syscall
    jmp hang   