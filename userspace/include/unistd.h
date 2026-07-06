#pragma once
#include <stdint.h>

unsigned int sleep(unsigned int seconds);

int   chdir(const char *path);
char *getcwd(char *buf, size_t size);

extern char *optarg;
extern int optind, opterr, optopt;

int getopt(int argc, char *const argv[], const char *optstring);
