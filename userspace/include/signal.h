#pragma once

#include <stdint.h>
#include <sys/syscall.h>

#define SIGINT (0)
#define SIGFPE (1)
#define SIGSEGV (2)
#define SIGILL (3)
#define SIGKILL (4)
#define SIGTTIN (5)

syscall_status_t set_signal_handler(uint8_t signal, void (*handler)(void *), void *user_ctx);
void signal_init(void);
