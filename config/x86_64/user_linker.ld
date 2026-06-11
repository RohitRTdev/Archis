OUTPUT_FORMAT("elf64-x86-64")
ENTRY(_start)

SECTIONS {
    .text ALIGN(0x1000): {
        KEEP(*(.text*))
    }

    .rodata ALIGN(0x1000): {
        *(.rodata*)
    }

    .data ALIGN(0x1000): {
        *(.data*)
    }

    .bss ALIGN(0x1000): {
        *(.bss*)
        *(COMMON)
    }

    .dynamic ALIGN(0x1000): {
        *(.dynamic)
    }

    .got ALIGN(0x1000): {
        *(.got.plt)
        *(.got)
    }

    .dynsym ALIGN(0x1000): {
        *(.dynsym)
    }

    .dynstr ALIGN(0x1000): {
        *(.dynstr)
    }

    .rela.dyn ALIGN(0x1000): {
        *(.rela.dyn)
        *(.rela*)
    }

    .plt ALIGN(0x1000): {
        *(.plt)
    }

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note*)
        *(.debug*)
        *(.gnu*)
    }
}
