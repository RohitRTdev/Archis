# Libc
Most of this library was written by claude. 
So far the functionality includes...

## `<string.h>` — Memory & String primitives
**Memory:** `memset`, `memcpy`, `memmove`, `memcmp`, `memchr`

**String:** `strlen`, `strcpy`, `strncpy`, `strcat`, `strncat`, `strcmp`, `strncmp`, `strchr`, `strrchr`, `strstr`

## `<stdio.h>` — Formatted output
`printf`, `vprintf`, `snprintf`, `vsnprintf`

Supports: `%d %i %u %x %X %p %c %s %%`, width and `0`-pad flags, `l`/`ll` length modifier for 64-bit values. Output goes through `sys_print`.

## `<stdlib.h>` — Memory allocation
`malloc`, `free`, `calloc`, `realloc`, `aligned_alloc`

Free-list allocator backed by `sys_allocate_memory` / `sys_deallocate_memory`. Requests 64 KB chunks from the kernel and sub-allocates with 16-byte alignment. Coalesces adjacent free blocks on `free`. 
## `<stdlib.h>` — String/number conversion
**String → number:** `atoi`, `atol`, `atoll`, `strtol`, `strtoul`, `strtoll`, `strtoull`

**Number → string:** `itoa`, `ltoa`, `ultoa`

## `<ctype.h>` — Character classification & conversion
**Classification:** `isdigit`, `isalpha`, `isalnum`, `isspace`, `isblank`, `isupper`, `islower`, `isprint`, `isgraph`, `ispunct`, `iscntrl`, `isxdigit`

**Conversion:** `toupper`, `tolower`

## `<stdarg.h>` — Variadic argument handling
`va_list`, `va_start`, `va_arg`, `va_end`, `va_copy` — thin wrappers over Clang `__builtin_va_*`.

## `<sys/syscall.h>` — Kernel syscall wrappers
`sys_print`, `sys_delay_ms`, `sys_close`, `sys_create_process`, `sys_create_thread`, `sys_resume_process`, `sys_set_session_id`, `sys_get_pid`, `sys_get_process_info`, `sys_allocate_memory`, `sys_deallocate_memory`

**Sync stubs (kernel impl pending):** `sys_create_sync_object(type, init_count, max_count, fd_out)`, `sys_wait(fd)`, `sys_signal(fd)` — `type` is `SYNC_SEMAPHORE` or `SYNC_EVENT` from `sync_type_t`.