#pragma once
#include <stdint.h>

void  malloc_init(void);
void *malloc(size_t size);
void  free(void *ptr);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);
void *aligned_alloc(size_t alignment, size_t size);

int       atoi(const char *s);
long      atol(const char *s);
long long atoll(const char *s);

long          strtol(const char *s, char **end, int base);
unsigned long strtoul(const char *s, char **end, int base);
long long     strtoll(const char *s, char **end, int base);
unsigned long long strtoull(const char *s, char **end, int base);

char *itoa(int n, char *buf, int base);
char *ltoa(long n, char *buf, int base);
char *ultoa(unsigned long n, char *buf, int base);
