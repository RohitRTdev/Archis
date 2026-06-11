#include "asm_macros.inc"

.extern arch_syscall_handler

// rcx = saved_rip, r11 = saved_rflags,
FUNC SYSCALL_ENTRY
    // First switch to kernel stack
    swapgs
    xchgq %rsp, %gs:16
    
    // Save all the callee saved registers
    pushq %gs:16
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    
    // Restore the actual user stack into this 
    leaq 56(%rsp), %r15
    movq %r15, %gs:16 

    // Save the return address and rflags
    pushq %rcx
    pushq %r11

    // Push all args along with syscall number
    // Make space to store user gs
    subq $8, %rsp
    pushq %r9
    pushq %r8
    pushq %r10 
    pushq %rdx 
    pushq %rsi 
    pushq %rdi 
    pushq %rax

    // To ensure 16 byte alignment
    subq $8, %rsp

    // Call the main handler with the arguments in place
    movq %rsp, %rdi
    call arch_syscall_handler

    // Done with syscall, restore return address and rflags
    addq $72, %rsp
    popq %r11
    popq %rcx

    // Restore all callee saved registers
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    
    // Restore caller stack
    popq %rsp
    
    // Restore user gs
    swapgs
    
    // Transfer control back to user
    sysretq
ENDF syscall_entry

// Param 1 = user_start_address, param 2 = init rflags, param 3 = user stack base
FUNC jump_to_user_code
    movq %rdi, %rcx
    movq %rsi, %r11
    movq %rdx, %rsp
    movq %rdx, %rbp
    
    sysretq
ENDF jump_to_user_code
