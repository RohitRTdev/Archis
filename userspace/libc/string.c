#include <string.h>

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}

char *strcpy(char *dest, const char *src) {
    char *d = dest;
    while ((*d++ = *src++));
    return dest;
}

char *strncpy(char *dest, const char *src, size_t n) {
    char *d = dest;
    while (n && (*d++ = *src++)) n--;
    while (n--) *d++ = '\0';
    return dest;
}

char *strcat(char *dest, const char *src) {
    char *d = dest;
    while (*d) d++;
    while ((*d++ = *src++));
    return dest;
}

char *strncat(char *dest, const char *src, size_t n) {
    char *d = dest;
    while (*d) d++;
    while (n-- && (*d++ = *src++));
    *d = '\0';
    return dest;
}

int strcmp(const char *s1, const char *s2) {
    while (*s1 && (*s1 == *s2)) { s1++; s2++; }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncmp(const char *s1, const char *s2, size_t n) {
    while (n-- && *s1 && (*s1 == *s2)) { s1++; s2++; }
    if (n == (size_t)-1) return 0;
    return (unsigned char)*s1 - (unsigned char)*s2;
}

char *strchr(const char *s, int c) {
    unsigned char ch = (unsigned char)c;
    while (*s) {
        if ((unsigned char)*s == ch) return (char *)s;
        s++;
    }
    return (ch == '\0') ? (char *)s : (char *)0;
}

char *strrchr(const char *s, int c) {
    unsigned char ch = (unsigned char)c;
    const char *last = (char *)0;
    while (*s) {
        if ((unsigned char)*s == ch) last = s;
        s++;
    }
    if (ch == '\0') return (char *)s;
    return (char *)last;
}

char *strstr(const char *haystack, const char *needle) {
    if (!*needle) return (char *)haystack;
    for (; *haystack; haystack++) {
        const char *h = haystack;
        const char *n = needle;
        while (*h && *n && *h == *n) { h++; n++; }
        if (!*n) return (char *)haystack;
    }
    return (char *)0;
}
