#pragma once
#include <stdint.h>
#include <stdarg.h>
#include <sys/syscall.h>

#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

#define EOF (-1)
#define BUFSIZ 256

typedef struct FILE {
    handle_t fd;               // -1 means not bound to a live handle
    unsigned char rbuf[BUFSIZ];
    size_t rpos, rlen;         // read-ahead buffer cursor/length
    int eof;
    int error;
    int ungetc_ch;             // pushed-back char, or -1 if empty
} FILE;

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

// Binds stdin/stdout/stderr to handles STDIN_FILENO/STDOUT_FILENO/STDERR_FILENO
// if those handles are currently live; otherwise leaves them unbound (fd == -1).
void stdio_init_std_handles(void);

int printf(const char *fmt, ...);
int vprintf(const char *fmt, va_list ap);
int snprintf(char *buf, size_t size, const char *fmt, ...);
int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);

FILE *fopen(const char *path, const char *mode);
int   fclose(FILE *stream);
int   fflush(FILE *stream);

size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);

int  fgetc(FILE *stream);
int  getc(FILE *stream);
int  fputc(int c, FILE *stream);
int  putc(int c, FILE *stream);
int  ungetc(int c, FILE *stream);

char *fgets(char *buf, int n, FILE *stream);
int   fputs(const char *s, FILE *stream);

int  feof(FILE *stream);
int  ferror(FILE *stream);
void clearerr(FILE *stream);

int     fseek(FILE *stream, long offset, int whence);
long    ftell(FILE *stream);
void    rewind(FILE *stream);

int remove(const char *path);

int scanf(const char *fmt, ...);
int vscanf(const char *fmt, va_list ap);
int fscanf(FILE *stream, const char *fmt, ...);
int vfscanf(FILE *stream, const char *fmt, va_list ap);
int sscanf(const char *str, const char *fmt, ...);
int vsscanf(const char *str, const char *fmt, va_list ap);
