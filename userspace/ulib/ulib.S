// ulib — shared userspace utility library
// Exports: ulib_hello, ulib_delay_100, ulib_get_magic
//
// Syscall convention (x86_64 sysret):
//   rax = syscall number
//   rdi = arg1, rsi = arg2

.text

// void ulib_hello(void)
// Prints a greeting via syscall 2 (write)
.globl ulib_hello
.type ulib_hello, @function
ulib_hello:
    pushq %rbx
    movq $2, %rax
    leaq .Lulib_hello_msg(%rip), %rdi
    movq $.Lulib_hello_msg_end - .Lulib_hello_msg, %rsi
    syscall
    popq %rbx
    ret
.size ulib_hello, . - ulib_hello

// void ulib_delay_100(void)
// Sleeps 100ms via syscall 3 (delay)
.globl ulib_delay_100
.type ulib_delay_100, @function
ulib_delay_100:
    pushq %rbx
    movq $3, %rax
    movq $100, %rdi
    syscall
    popq %rbx
    ret
.size ulib_delay_100, . - ulib_delay_100

// uint64_t ulib_get_magic(void)
// Returns 0xCAFE — callers can verify linking worked
.globl ulib_get_magic
.type ulib_get_magic, @function
ulib_get_magic:
    movq $0xCAFE, %rax
    ret
.size ulib_get_magic, . - ulib_get_magic

.section .rodata
.Lulib_hello_msg:
    .ascii "ulib: hello from shared lib!"
.Lulib_hello_msg_end:
