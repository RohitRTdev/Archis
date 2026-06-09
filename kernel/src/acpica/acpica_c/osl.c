// Minimal printf/vprintf for ACPICA. We deliberately keep this self-contained
// (no libc) and only support the subset of conversions ACPICA actually emits:
//
//   - flags     : '-' (left align), '0' (zero pad), ' ', '+', '#'
//   - width     : decimal digits   (or '*' — read from args)
//   - precision : '.' decimal      (or '.*' — read from args)
//   - length    : 'l', 'll', 'L', 'z', 'h', 'hh'
//   - specifier : 'd', 'i', 'u', 'x', 'X', 'o', 'p', 's', 'c', '%'
//
// Output is rendered into a fixed buffer and handed to AcpiOsPrintStr — the
// kernel-side sink — exactly once per call.

#include "acpi.h"

extern void AcpiOsPrintStr(const char* s);

#define PRINTF_BUF_SIZE 512

typedef struct {
    char *buf;
    int   pos;
    int   cap;
} Out;

static void out_putc(Out *o, char c) {
    if (o->pos < o->cap - 1) {
        o->buf[o->pos++] = c;
    }
}

static void out_repeat(Out *o, char c, int n) {
    while (n-- > 0) out_putc(o, c);
}

static void out_str(Out *o, const char *s, int len) {
    for (int i = 0; i < len; i++) out_putc(o, s[i]);
}

// Render a signed/unsigned integer into a small temp buffer (LSB first), then
// pad and emit. Width/precision/flags follow standard C printf semantics for
// the subset we support.
static void emit_int(Out *o,
                     unsigned long long value,
                     int is_negative,
                     int base,
                     int upper,
                     int width,
                     int precision,
                     int left_align,
                     int zero_pad,
                     int show_sign,
                     int space_sign,
                     int alt_form)
{
    char tmp[32];
    int  ti = 0;

    if (value == 0 && precision == 0) {
        // C99: explicit zero precision with value 0 prints nothing.
    } else {
        do {
            unsigned int digit = (unsigned int)(value % (unsigned)base);
            char ch;
            if (digit < 10) ch = (char)('0' + digit);
            else            ch = (char)((upper ? 'A' : 'a') + (digit - 10));
            tmp[ti++] = ch;
            value /= (unsigned)base;
        } while (value && ti < (int)sizeof(tmp));
    }

    int num_digits = ti;
    if (precision > num_digits) num_digits = precision;

    // Compose prefix (sign, alt form for hex/octal).
    char prefix[4];
    int  plen = 0;
    if (is_negative)        prefix[plen++] = '-';
    else if (show_sign)     prefix[plen++] = '+';
    else if (space_sign)    prefix[plen++] = ' ';
    if (alt_form && base == 16) {
        prefix[plen++] = '0';
        prefix[plen++] = (char)(upper ? 'X' : 'x');
    } else if (alt_form && base == 8) {
        prefix[plen++] = '0';
    }

    // Field width includes prefix + (precision-extended) digits.
    int body_len = plen + num_digits;
    int pad      = width > body_len ? width - body_len : 0;

    if (!left_align && !zero_pad)
        out_repeat(o, ' ', pad);

    out_str(o, prefix, plen);

    if (!left_align && zero_pad && precision < 0)
        out_repeat(o, '0', pad);

    // Precision zero-padding for digit count.
    out_repeat(o, '0', num_digits - ti);

    // Emit the actual digits, reversed.
    while (ti-- > 0) out_putc(o, tmp[ti]);

    if (left_align)
        out_repeat(o, ' ', pad);
}

static int parse_int(const char **p) {
    int v = 0;
    while (**p >= '0' && **p <= '9') {
        v = v * 10 + (**p - '0');
        ++(*p);
    }
    return v;
}

void AcpiOsVprintf(const char *fmt, va_list args) {
    char buf[PRINTF_BUF_SIZE];
    Out  o = { buf, 0, PRINTF_BUF_SIZE };

    while (*fmt) {
        if (*fmt != '%') {
            out_putc(&o, *fmt++);
            continue;
        }
        ++fmt;
        if (!*fmt) break;

        // flags
        int left_align = 0, zero_pad = 0, show_sign = 0, space_sign = 0, alt_form = 0;
        while (1) {
            if (*fmt == '-')      { left_align = 1; ++fmt; }
            else if (*fmt == '0') { zero_pad   = 1; ++fmt; }
            else if (*fmt == '+') { show_sign  = 1; ++fmt; }
            else if (*fmt == ' ') { space_sign = 1; ++fmt; }
            else if (*fmt == '#') { alt_form   = 1; ++fmt; }
            else break;
        }

        // width 
        int width = 0;
        if (*fmt == '*') {
            width = va_arg(args, int);
            if (width < 0) { left_align = 1; width = -width; }
            ++fmt;
        } else {
            width = parse_int(&fmt);
        }

        // precision
        int precision = -1;
        if (*fmt == '.') {
            ++fmt;
            if (*fmt == '*') {
                precision = va_arg(args, int);
                if (precision < 0) precision = -1;
                ++fmt;
            } else {
                precision = parse_int(&fmt);
            }
        }

        // length modifier
        int length = 0; // 0=int, 1=long, 2=long long, -1=short, -2=char
        if (*fmt == 'l') {
            ++fmt;
            length = 1;
            if (*fmt == 'l') { ++fmt; length = 2; }
        } else if (*fmt == 'L') {
            ++fmt; length = 2;
        } else if (*fmt == 'z') {
            ++fmt; length = 1;
        } else if (*fmt == 'h') {
            ++fmt; length = -1;
            if (*fmt == 'h') { ++fmt; length = -2; }
        }

        // Left-align flag suppresses zero-pad per C99.
        if (left_align) zero_pad = 0;

        char spec = *fmt;
        if (!spec) break;
        ++fmt;

        switch (spec) {
            case '%':
                out_putc(&o, '%');
                break;

            case 'c': {
                char c = (char)va_arg(args, int);
                int pad = width > 1 ? width - 1 : 0;
                if (!left_align) out_repeat(&o, ' ', pad);
                out_putc(&o, c);
                if (left_align)  out_repeat(&o, ' ', pad);
                break;
            }

            case 's': {
                const char *s = va_arg(args, const char*);
                if (!s) s = "(null)";
                int slen = 0;
                while (s[slen]) ++slen;
                if (precision >= 0 && precision < slen) slen = precision;
                int pad = width > slen ? width - slen : 0;
                if (!left_align) out_repeat(&o, ' ', pad);
                out_str(&o, s, slen);
                if (left_align)  out_repeat(&o, ' ', pad);
                break;
            }

            case 'd': case 'i': {
                long long sv;
                switch (length) {
                    case 2:  sv = va_arg(args, long long); break;
                    case 1:  sv = va_arg(args, long);      break;
                    default: sv = va_arg(args, int);       break;
                }
                int neg = sv < 0;
                unsigned long long uv = (unsigned long long)(neg ? -sv : sv);
                emit_int(&o, uv, neg, 10, 0, width, precision,
                         left_align, zero_pad, show_sign, space_sign, 0);
                break;
            }

            case 'u': case 'o': case 'x': case 'X': case 'p': {
                unsigned long long uv;
                int base  = 10;
                int upper = 0;
                int alt   = alt_form;

                if (spec == 'p') {
                    // Pointer: hex with 0x prefix, no flag-driven width unless caller asked.
                    uv = (unsigned long long)(unsigned long)va_arg(args, void*);
                    base = 16;
                    alt  = 1;
                } else {
                    if (spec == 'o')                  base = 8;
                    else if (spec == 'x')             base = 16;
                    else if (spec == 'X') { base = 16; upper = 1; }

                    switch (length) {
                        case 2:  uv = va_arg(args, unsigned long long); break;
                        case 1:  uv = va_arg(args, unsigned long);      break;
                        default: uv = (unsigned long)va_arg(args, unsigned int); break;
                    }
                }
                emit_int(&o, uv, 0, base, upper, width, precision,
                         left_align, zero_pad, 0, 0, alt);
                break;
            }

            default:
                // Unknown specifier — echo it so we don't silently swallow.
                out_putc(&o, '%');
                out_putc(&o, spec);
                break;
        }
    }

    o.buf[o.pos] = 0;
    AcpiOsPrintStr(o.buf);
}

void AcpiOsPrintf(const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    AcpiOsVprintf(fmt, args);
    va_end(args);
}
