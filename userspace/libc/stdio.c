#include <stdio.h>
#include <sys/syscall.h>

static void out_char(char *buf, size_t size, size_t *pos, char c) {
    if (*pos < size) buf[*pos] = c;
    (*pos)++;
}

static void out_str(char *buf, size_t size, size_t *pos, const char *s) {
    while (*s) out_char(buf, size, pos, *s++);
}

static void out_uint(char *buf, size_t size, size_t *pos, uint64_t val, int base, int upper, int width, int zero_pad) {
    char tmp[32];
    int i = 0;
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";

    if (val == 0) {
        tmp[i++] = '0';
    }
    while (val) {
        tmp[i++] = digits[val % (uint64_t)base];
        val /= (uint64_t)base;
    }

    int pad = width - i;
    while (pad-- > 0) out_char(buf, size, pos, zero_pad ? '0' : ' ');
    while (i--) out_char(buf, size, pos, tmp[i]);
}

static void out_int(char *buf, size_t size, size_t *pos, int64_t val, int width, int zero_pad) {
    if (val < 0) {
        out_char(buf, size, pos, '-');
        out_uint(buf, size, pos, (uint64_t)(-val), 10, 0, width > 0 ? width - 1 : 0, zero_pad);
    } else {
        out_uint(buf, size, pos, (uint64_t)val, 10, 0, width, zero_pad);
    }
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap) {
    size_t pos = 0;

    while (*fmt) {
        if (*fmt != '%') {
            out_char(buf, size, &pos, *fmt++);
            continue;
        }

        fmt++;

        int zero_pad = 0;
        int width = 0;
        if (*fmt == '0') { zero_pad = 1; fmt++; }
        while (*fmt >= '0' && *fmt <= '9') { width = width * 10 + (*fmt - '0'); fmt++; }

        int is_long = 0;
        if (*fmt == 'l') {
            is_long = 1;
            fmt++;
            if (*fmt == 'l') fmt++;
        }

        switch (*fmt) {
            case 'd':
            case 'i': {
                int64_t val = is_long ? va_arg(ap, int64_t) : va_arg(ap, int);
                out_int(buf, size, &pos, val, width, zero_pad);
                break;
            }
            case 'u': {
                uint64_t val = is_long ? va_arg(ap, uint64_t) : va_arg(ap, unsigned int);
                out_uint(buf, size, &pos, val, 10, 0, width, zero_pad);
                break;
            }
            case 'x':
            case 'X': {
                uint64_t val = is_long ? va_arg(ap, uint64_t) : va_arg(ap, unsigned int);
                out_uint(buf, size, &pos, val, 16, *fmt == 'X', width, zero_pad);
                break;
            }
            case 'p': {
                uintptr_t val = (uintptr_t)va_arg(ap, void *);
                out_str(buf, size, &pos, "0x");
                out_uint(buf, size, &pos, val, 16, 0, 16, 1);
                break;
            }
            case 'c': {
                char c = (char)va_arg(ap, int);
                out_char(buf, size, &pos, c);
                break;
            }
            case 's': {
                const char *s = va_arg(ap, const char *);
                out_str(buf, size, &pos, s ? s : "(null)");
                break;
            }
            case '%': {
                out_char(buf, size, &pos, '%');
                break;
            }
            default: {
                out_char(buf, size, &pos, '%');
                if (*fmt) out_char(buf, size, &pos, *fmt);
                break;
            }
        }

        if (*fmt) fmt++;
    }

    if (size > 0) {
        buf[pos < size ? pos : size - 1] = '\0';
    }

    return (int)pos;
}

int snprintf(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return ret;
}

int vprintf(const char *fmt, va_list ap) {
    char buf[512];
    int ret = vsnprintf(buf, sizeof(buf), fmt, ap);
    if (ret > 0) {
        size_t bw = 0;
        size_t len = (ret < (int)sizeof(buf)) ? (size_t)ret : sizeof(buf) - 1;
        sys_write(STDOUT_FILENO, buf, len, &bw);
    }
    return ret;
}

int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vprintf(fmt, ap);
    va_end(ap);
    return ret;
}

unsigned int sleep(unsigned int seconds) {
    uint64_t old = 0, now = 0;
    sys_get_time_ms(CLOCK_MONOTONIC, &old);
    syscall_status_t res = sys_delay_ms(seconds * 1000);
    if (res != E_SUCCESS) {
        sys_get_time_ms(CLOCK_MONOTONIC, &now);
        return (now - old) / 1000;
    }

    return 0;
}
