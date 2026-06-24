#include <stdlib.h>

static int is_space(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v';
}

static int digit_val(char c, int base) {
    int val;
    if (c >= '0' && c <= '9')      val = c - '0';
    else if (c >= 'a' && c <= 'z') val = c - 'a' + 10;
    else if (c >= 'A' && c <= 'Z') val = c - 'A' + 10;
    else return -1;
    return val < base ? val : -1;
}

static unsigned long long parse_ull(const char *s, char **end, int base, int *neg) {
    while (is_space(*s)) s++;

    *neg = 0;
    if (*s == '-') { *neg = 1; s++; }
    else if (*s == '+') { s++; }

    if (base == 0) {
        if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) base = 16;
        else if (s[0] == '0')                              base = 8;
        else                                               base = 10;
    }

    if (base == 16 && s[0] == '0' && (s[1] == 'x' || s[1] == 'X'))
        s += 2;

    unsigned long long result = 0;
    int any = 0;
    int d;
    while ((d = digit_val(*s, base)) >= 0) {
        result = result * (unsigned long long)base + (unsigned long long)d;
        s++;
        any = 1;
    }

    if (end) *end = (char *)(any ? s : (s - ((*neg || *s == '+') ? 1 : 0)));
    return result;
}

long strtol(const char *s, char **end, int base) {
    int neg;
    unsigned long long val = parse_ull(s, end, base, &neg);
    return neg ? -(long)val : (long)val;
}

unsigned long strtoul(const char *s, char **end, int base) {
    int neg;
    unsigned long long val = parse_ull(s, end, base, &neg);
    return neg ? -(unsigned long)val : (unsigned long)val;
}

long long strtoll(const char *s, char **end, int base) {
    int neg;
    unsigned long long val = parse_ull(s, end, base, &neg);
    return neg ? -(long long)val : (long long)val;
}

unsigned long long strtoull(const char *s, char **end, int base) {
    int neg;
    unsigned long long val = parse_ull(s, end, base, &neg);
    return neg ? -val : val;
}

int atoi(const char *s) {
    return (int)strtol(s, (char **)0, 10);
}

long atol(const char *s) {
    return strtol(s, (char **)0, 10);
}

long long atoll(const char *s) {
    return strtoll(s, (char **)0, 10);
}

static char *ull_to_str(unsigned long long val, char *buf, int base, int upper) {
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    int i = 0;

    if (val == 0) {
        buf[i++] = '0';
    }
    while (val) {
        buf[i++] = digits[val % (unsigned long long)base];
        val /= (unsigned long long)base;
    }

    /* reverse in place */
    for (int l = 0, r = i - 1; l < r; l++, r--) {
        char tmp = buf[l]; buf[l] = buf[r]; buf[r] = tmp;
    }
    buf[i] = '\0';
    return buf;
}

char *itoa(int n, char *buf, int base) {
    int i = 0;
    unsigned int u;
    if (n < 0 && base == 10) { buf[i++] = '-'; u = (unsigned int)(-(n + 1)) + 1; }
    else { u = (unsigned int)n; }
    ull_to_str((unsigned long long)u, buf + i, base, 0);
    return buf;
}

char *ltoa(long n, char *buf, int base) {
    int i = 0;
    unsigned long u;
    if (n < 0 && base == 10) { buf[i++] = '-'; u = (unsigned long)(-(n + 1)) + 1; }
    else { u = (unsigned long)n; }
    ull_to_str((unsigned long long)u, buf + i, base, 0);
    return buf;
}

char *ultoa(unsigned long n, char *buf, int base) {
    ull_to_str((unsigned long long)n, buf, base, 0);
    return buf;
}

