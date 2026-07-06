#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <string.h>
#include <stdlib.h>

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

int vfprintf(FILE *stream, const char *fmt, va_list ap) {
    char buf[512];
    int ret = vsnprintf(buf, sizeof(buf), fmt, ap);
    if (ret > 0 && stream && stream->fd >= 0) {
        size_t bw = 0;
        size_t len = (ret < (int)sizeof(buf)) ? (size_t)ret : sizeof(buf) - 1;
        sys_write(stream->fd, buf, len, &bw);
    }
    return ret;
}

int fprintf(FILE *stream, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vfprintf(stream, fmt, ap);
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

static FILE g_stdin  = { .fd = -1, .ungetc_ch = -1 };
static FILE g_stdout = { .fd = -1, .ungetc_ch = -1 };
static FILE g_stderr = { .fd = -1, .ungetc_ch = -1 };

FILE *stdin  = &g_stdin;
FILE *stdout = &g_stdout;
FILE *stderr = &g_stderr;

static void bind_std_handle(FILE *f, handle_t h) {
    file_stat_t st;
    f->rpos = f->rlen = 0;
    f->eof = 0;
    f->error = 0;
    f->ungetc_ch = -1;
    f->fd = (sys_fstat(h, &st) == E_SUCCESS) ? h : -1;
}

void stdio_init_std_handles(void) {
    bind_std_handle(stdin, STDIN_FILENO);
    bind_std_handle(stdout, STDOUT_FILENO);
    bind_std_handle(stderr, STDERR_FILENO);
}

static int refill(FILE *stream) {
    if (stream->fd < 0) return -1;
    size_t n = 0;
    if (sys_read(stream->fd, stream->rbuf, sizeof(stream->rbuf), &n) != E_SUCCESS) {
        stream->error = 1;
        return -1;
    }
    if (n == 0) {
        stream->eof = 1;
        return -1;
    }
    stream->rpos = 0;
    stream->rlen = n;
    return 0;
}

int fgetc(FILE *stream) {
    if (!stream) return EOF;
    if (stream->ungetc_ch >= 0) {
        int c = stream->ungetc_ch;
        stream->ungetc_ch = -1;
        return c;
    }
    if (stream->rpos >= stream->rlen && refill(stream) < 0) {
        return EOF;
    }
    return stream->rbuf[stream->rpos++];
}

int getc(FILE *stream) {
    return fgetc(stream);
}

int ungetc(int c, FILE *stream) {
    if (!stream || c == EOF) return EOF;
    stream->ungetc_ch = (unsigned char)c;
    return (unsigned char)c;
}

int fputc(int c, FILE *stream) {
    if (!stream || stream->fd < 0) return EOF;
    unsigned char ch = (unsigned char)c;
    size_t written = 0;
    if (sys_write(stream->fd, &ch, 1, &written) != E_SUCCESS || written != 1) {
        stream->error = 1;
        return EOF;
    }
    return ch;
}

int putc(int c, FILE *stream) {
    return fputc(c, stream);
}

char *fgets(char *buf, int n, FILE *stream) {
    if (!buf || n <= 0 || !stream) return NULL;
    int i = 0;
    while (i < n - 1) {
        int c = fgetc(stream);
        if (c == EOF) {
            if (i == 0) return NULL;
            break;
        }
        buf[i++] = (char)c;
        if (c == '\n') break;
    }
    buf[i] = '\0';
    return buf;
}

int fputs(const char *s, FILE *stream) {
    if (!s || !stream || stream->fd < 0) return EOF;
    size_t len = strlen(s);
    size_t written = 0;
    if (len > 0 && (sys_write(stream->fd, s, len, &written) != E_SUCCESS || written != len)) {
        stream->error = 1;
        return EOF;
    }
    return 0;
}

int feof(FILE *stream) {
    return stream ? stream->eof : 0;
}

int ferror(FILE *stream) {
    return stream ? stream->error : 0;
}

void clearerr(FILE *stream) {
    if (!stream) return;
    stream->eof = 0;
    stream->error = 0;
}

FILE *fopen(const char *path, const char *mode) {
    if (!path || !mode || !mode[0]) return NULL;

    int want_append = 0, want_truncate = 0;
    switch (mode[0]) {
        case 'r': break;
        case 'w': want_truncate = 1; break;
        case 'a': want_append = 1; break;
        default: return NULL;
    }

    handle_t h;
    if (want_truncate) {
        // OPEN_CREATE_FLAG set: sys_open's "fs" handler always deletes+recreates.
        h = sys_open("fs", path, OPEN_CREATE_FLAG);
    } else if (want_append) {
        // Open without truncating if the file already exists (preserve its
        // contents for appending); only fall back to create-fresh if it's missing.
        h = sys_open("fs", path, 0);
        if (h < 0) {
            h = sys_open("fs", path, OPEN_CREATE_FLAG);
        }
    } else {
        h = sys_open("fs", path, 0);
    }
    if (h < 0) return NULL;

    if (want_append) {
        sys_seek(h, 0, SEEK_END);
    }

    FILE *f = malloc(sizeof(FILE));
    if (!f) {
        sys_close(h);
        return NULL;
    }
    f->fd = h;
    f->rpos = f->rlen = 0;
    f->eof = 0;
    f->error = 0;
    f->ungetc_ch = -1;
    return f;
}

int fclose(FILE *stream) {
    if (!stream) return EOF;
    int ret = 0;
    if (stream->fd >= 0 && sys_close(stream->fd) != E_SUCCESS) {
        ret = EOF;
    }
    stream->fd = -1;
    if (stream != stdin && stream != stdout && stream != stderr) {
        free(stream);
    }
    return ret;
}

int fflush(FILE *stream) {
    (void)stream;
    return 0;
}

size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream) {
    if (!ptr || !stream || size == 0 || nmemb == 0 || stream->fd < 0) return 0;

    unsigned char *dst = (unsigned char *)ptr;
    size_t total = size * nmemb;
    size_t copied = 0;

    if (stream->rpos < stream->rlen) {
        size_t avail = stream->rlen - stream->rpos;
        size_t take = avail < total ? avail : total;
        memcpy(dst, stream->rbuf + stream->rpos, take);
        stream->rpos += take;
        copied += take;
    }

    while (copied < total) {
        size_t n = 0;
        if (sys_read(stream->fd, dst + copied, total - copied, &n) != E_SUCCESS) {
            stream->error = 1;
            break;
        }
        if (n == 0) {
            stream->eof = 1;
            break;
        }
        copied += n;
    }

    return copied / size;
}

size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream) {
    if (!ptr || !stream || size == 0 || nmemb == 0 || stream->fd < 0) return 0;

    const unsigned char *src = (const unsigned char *)ptr;
    size_t total = size * nmemb;
    size_t written = 0;

    while (written < total) {
        size_t n = 0;
        if (sys_write(stream->fd, src + written, total - written, &n) != E_SUCCESS || n == 0) {
            stream->error = 1;
            break;
        }
        written += n;
    }

    return written / size;
}

long ftell(FILE *stream) {
    if (!stream || stream->fd < 0) return -1;
    ssize_t pos = sys_seek(stream->fd, 0, SEEK_CUR);
    if (pos < 0) return -1;
    size_t buffered_unread = stream->rlen - stream->rpos;
    if (stream->ungetc_ch >= 0) buffered_unread += 1;
    return (long)(pos - (ssize_t)buffered_unread);
}

int fseek(FILE *stream, long offset, int whence) {
    if (!stream || stream->fd < 0) return -1;

    ssize_t target;
    if (whence == SEEK_SET) {
        target = offset;
    } else if (whence == SEEK_CUR) {
        long cur = ftell(stream);
        if (cur < 0) return -1;
        target = cur + offset;
    } else if (whence == SEEK_END) {
        file_stat_t st;
        if (sys_fstat(stream->fd, &st) != E_SUCCESS) return -1;
        target = (ssize_t)st.size + offset;
    } else {
        return -1;
    }

    if (sys_seek(stream->fd, target, SEEK_SET) < 0) return -1;

    stream->rpos = stream->rlen = 0;
    stream->ungetc_ch = -1;
    stream->eof = 0;
    return 0;
}

void rewind(FILE *stream) {
    fseek(stream, 0, SEEK_SET);
    if (stream) stream->error = 0;
}

int remove(const char *path) {
    return sys_delete(path) == E_SUCCESS ? 0 : -1;
}
