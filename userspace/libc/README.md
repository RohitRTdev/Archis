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

## `<pthread.h>` — Mutex and condition variables

**Mutex:** `pthread_mutex_init`, `pthread_mutex_destroy`, `pthread_mutex_lock`, `pthread_mutex_unlock`, `pthread_mutex_trylock`

**Condition variables:** `pthread_cond_init`, `pthread_cond_destroy`, `pthread_cond_wait`, `pthread_cond_timedwait`, `pthread_cond_signal`, `pthread_cond_broadcast`

Return values follow POSIX: 0 on success, positive error code (`EINVAL`, `EBUSY`, `ENOMEM`, `ETIMEDOUT`) on failure.

## `<semaphore.h>` — POSIX semaphores

`sem_init`, `sem_destroy`, `sem_wait`, `sem_post`, `sem_trywait`

Direct wrappers over the kernel `SYNC_SEMAPHORE` primitive. `sem_init` ignores the `pshared` argument (cross-process semaphores are not yet supported). Return 0 on success, -1 on failure (POSIX convention).

## `<sys/syscall.h>` — Kernel syscall wrappers

`sys_print`, `sys_delay_ms`, `sys_close`, `sys_create_process`, `sys_create_thread`, `sys_resume_process`, `sys_set_session_leader`, `sys_get_pid`, `sys_get_process_info`, `sys_allocate_memory`, `sys_deallocate_memory`

**Sync primitives:** `sys_create_sync_object(type, init_count, max_count, auto_reset)`, `sys_wait(fd, timeout_ms)`, `sys_signal(fd)` — `type` is `SYNC_SEMAPHORE` or `SYNC_EVENT`. `timeout_ms = -1` waits indefinitely; `timeout_ms = 0` is a non-blocking poll that returns `E_TIMEOUT` if the object is not immediately available.

**Time:** `sys_get_time_ms(uint64_t *out)` — writes milliseconds since boot to `*out`

**Error codes:** `E_SUCCESS (0)`, `E_INVALID (-1)`, `E_OOM (-2)`, `E_INTERNAL_FAILURE (-3)`, `E_NOT_SUPPORTED (-4)`, `E_DEV_STOPPED (-5)`, `E_INVALID_MEMORY_RANGE (-6)`, `E_PROCESS_TERMINATED (-7)`, `E_NOPERM (-8)`, `E_WAIT_INTERRUPTED (-11)`, `E_TIMEOUT (-12)`