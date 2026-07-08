.extern kern_start
.global KERNEL_STACK
.global KERNEL_STACK_TOP

.section .bss
.balign 4096

KERNEL_STACK:
    .skip 4096 * 10
KERNEL_STACK_TOP:

#include "asm_macros.inc"

// Setup stack and jump to kernel entry point in rust
.section .text
FUNC kern_start_asm
    cli
    leaq KERNEL_STACK_TOP(%rip), %rsp
    movq %rsp, %rbp
    jmp kern_start
ENDF kern_start_asm