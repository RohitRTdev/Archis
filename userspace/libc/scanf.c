#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <stdarg.h>

// Length modifiers recognized in numeric conversions.
enum { LEN_NONE, LEN_CHAR, LEN_SHORT, LEN_LONG, LEN_LONGLONG };

typedef struct {
    int (*get)(void *ctx);
    void (*unget)(void *ctx, int c);
    void *ctx;
} scan_src_t;

static int file_get(void *ctx) {
    return fgetc((FILE *)ctx);
}

static void file_unget(void *ctx, int c) {
    if (c != EOF) ungetc(c, (FILE *)ctx);
}

typedef struct {
    const char *s;
    size_t pos;
} str_ctx_t;

static int str_get(void *ctx) {
    str_ctx_t *sc = (str_ctx_t *)ctx;
    unsigned char c = (unsigned char)sc->s[sc->pos];
    if (c == '\0') return EOF;
    sc->pos++;
    return c;
}

static void str_unget(void *ctx, int c) {
    (void)c;
    str_ctx_t *sc = (str_ctx_t *)ctx;
    if (sc->pos > 0) sc->pos--;
}

static int matches_digit(char conv, int c) {
    if (conv == 'x') return isxdigit(c);
    if (conv == 'i') return isalnum(c);
    return isdigit(c);
}

static int vformat_scan(scan_src_t *src, const char *fmt, va_list ap) {
    int assigned = 0;
    int any_input = 0;
    int c;

    while (*fmt) {
        if (isspace((unsigned char)*fmt)) {
            while ((c = src->get(src->ctx)) != EOF && isspace((unsigned char)c)) {
                any_input = 1;
            }
            if (c != EOF) src->unget(src->ctx, c);
            fmt++;
            continue;
        }

        if (*fmt != '%') {
            c = src->get(src->ctx);
            if (c == EOF) return (any_input || assigned) ? assigned : EOF;
            any_input = 1;
            if ((char)c != *fmt) {
                src->unget(src->ctx, c);
                return assigned;
            }
            fmt++;
            continue;
        }

        fmt++;
        if (*fmt == '%') {
            c = src->get(src->ctx);
            if (c == EOF) return (any_input || assigned) ? assigned : EOF;
            any_input = 1;
            if (c != '%') {
                src->unget(src->ctx, c);
                return assigned;
            }
            fmt++;
            continue;
        }

        int suppress = 0;
        if (*fmt == '*') {
            suppress = 1;
            fmt++;
        }

        int width = 0;
        while (*fmt >= '0' && *fmt <= '9') {
            width = width * 10 + (*fmt - '0');
            fmt++;
        }

        int len_mod = LEN_NONE;
        if (*fmt == 'h') {
            fmt++;
            if (*fmt == 'h') { len_mod = LEN_CHAR; fmt++; }
            else len_mod = LEN_SHORT;
        } else if (*fmt == 'l') {
            fmt++;
            if (*fmt == 'l') { len_mod = LEN_LONGLONG; fmt++; }
            else len_mod = LEN_LONG;
        }

        char conv = *fmt;
        if (!conv) break;
        fmt++;

        if (conv == 'c') {
            int w = width > 0 ? width : 1;
            char *out = suppress ? NULL : va_arg(ap, char *);
            int got = 0;
            for (int i = 0; i < w; i++) {
                c = src->get(src->ctx);
                if (c == EOF) break;
                any_input = 1;
                if (out) out[i] = (char)c;
                got++;
            }
            if (got < w) return (any_input || assigned) ? assigned : EOF;
            if (!suppress) assigned++;
            continue;
        }

        do {
            c = src->get(src->ctx);
        } while (c != EOF && isspace((unsigned char)c));
        if (c == EOF) return (any_input || assigned) ? assigned : EOF;
        any_input = 1;

        if (conv == 's') {
            char tmp[256];
            int i = 0;
            int maxw = width > 0 ? width : (int)sizeof(tmp) - 1;
            while (c != EOF && !isspace((unsigned char)c) && i < maxw && i < (int)sizeof(tmp) - 1) {
                tmp[i++] = (char)c;
                c = src->get(src->ctx);
            }
            if (c != EOF) src->unget(src->ctx, c);
            tmp[i] = '\0';
            if (i == 0) return assigned;
            if (!suppress) {
                char *out = va_arg(ap, char *);
                memcpy(out, tmp, i + 1);
                assigned++;
            }
            continue;
        }

        // Numeric conversions: %d %i %u %x %o
        char tok[64];
        int ti = 0;
        if (c == '+' || c == '-') {
            tok[ti++] = (char)c;
            c = src->get(src->ctx);
        }
        if ((conv == 'x' || conv == 'i') && c == '0') {
            tok[ti++] = (char)c;
            c = src->get(src->ctx);
            if (c == 'x' || c == 'X') {
                tok[ti++] = (char)c;
                c = src->get(src->ctx);
            }
        }

        int maxw = width > 0 ? width : (int)sizeof(tok) - 1;
        while (c != EOF && ti < maxw && ti < (int)sizeof(tok) - 1 && matches_digit(conv, c)) {
            tok[ti++] = (char)c;
            c = src->get(src->ctx);
        }
        if (c != EOF) src->unget(src->ctx, c);
        tok[ti] = '\0';

        if (ti == 0 || (ti == 1 && (tok[0] == '+' || tok[0] == '-'))) {
            return assigned;
        }

        int base = 10;
        if (conv == 'x') base = 16;
        else if (conv == 'o') base = 8;
        else if (conv == 'i') base = 0;

        if (conv == 'd' || conv == 'i') {
            long val = strtol(tok, NULL, base);
            if (!suppress) {
                switch (len_mod) {
                    case LEN_CHAR: *va_arg(ap, signed char *) = (signed char)val; break;
                    case LEN_SHORT: *va_arg(ap, short *) = (short)val; break;
                    case LEN_LONG: *va_arg(ap, long *) = val; break;
                    case LEN_LONGLONG: *va_arg(ap, long long *) = (long long)val; break;
                    default: *va_arg(ap, int *) = (int)val; break;
                }
            }
        } else {
            unsigned long val = strtoul(tok, NULL, base);
            if (!suppress) {
                switch (len_mod) {
                    case LEN_CHAR: *va_arg(ap, unsigned char *) = (unsigned char)val; break;
                    case LEN_SHORT: *va_arg(ap, unsigned short *) = (unsigned short)val; break;
                    case LEN_LONG: *va_arg(ap, unsigned long *) = val; break;
                    case LEN_LONGLONG: *va_arg(ap, unsigned long long *) = (unsigned long long)val; break;
                    default: *va_arg(ap, unsigned int *) = (unsigned int)val; break;
                }
            }
        }
        if (!suppress) assigned++;
    }

    return assigned;
}

int vfscanf(FILE *stream, const char *fmt, va_list ap) {
    scan_src_t src = { file_get, file_unget, stream };
    return vformat_scan(&src, fmt, ap);
}

int fscanf(FILE *stream, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vfscanf(stream, fmt, ap);
    va_end(ap);
    return ret;
}

int vscanf(const char *fmt, va_list ap) {
    return vfscanf(stdin, fmt, ap);
}

int scanf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vscanf(fmt, ap);
    va_end(ap);
    return ret;
}

int vsscanf(const char *str, const char *fmt, va_list ap) {
    str_ctx_t sc = { str, 0 };
    scan_src_t src = { str_get, str_unget, &sc };
    return vformat_scan(&src, fmt, ap);
}

int sscanf(const char *str, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vsscanf(str, fmt, ap);
    va_end(ap);
    return ret;
}
