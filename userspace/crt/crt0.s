.global _start
.extern main

.text

_start:
    call main

hang:
    jmp hang
