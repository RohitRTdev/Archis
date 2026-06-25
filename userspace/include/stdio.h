#pragma once
#include <stdint.h>
#include <stdarg.h>

#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FINENO 2

int printf(const char *fmt, ...);
int vprintf(const char *fmt, va_list ap);
int snprintf(char *buf, size_t size, const char *fmt, ...);
int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);
