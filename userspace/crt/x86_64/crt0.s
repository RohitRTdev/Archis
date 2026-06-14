.global _start
.extern main

.text

_start:
    call main
    call exit

hang:
    jmp hang


exit:
    movslq %eax, %rdi
    movq $0, %rax
    syscall
    jmp hang   