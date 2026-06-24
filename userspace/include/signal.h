#pragma once

#include <stdint.h>
#include <sys/syscall.h>

#define SIGFPE 0
#define SIGSEGV 1
#define SIGILL 2
#define SIGKILL 3

syscall_status_t set_signal_handler(uint8_t signal, void (*handler)(void *), void *user_ctx);
