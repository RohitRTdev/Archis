#pragma once
#include <stdint.h>

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
