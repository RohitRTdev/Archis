#include <stdio.h>
#include <unistd.h>
#include <getopt.h>

static void print_help(void) {
    printf("Usage: echo [-n] [-e] [-E] [-h] [string ...]\n");
    printf("Write arguments to standard output.\n\n");
    printf("  -n        do not output a trailing newline\n");
    printf("  -e        enable interpretation of backslash escapes\n");
    printf("  -E        disable interpretation of backslash escapes (default)\n");
    printf("  -h, --help  display this help and exit\n\n");
    printf("Recognized escapes when -e is used: \\\\ \\n \\t \\r \\a \\b \\f \\v \\c\n");
    printf("(\\c stops all further output immediately, with no trailing newline)\n");
}

static void output_escaped(const char *s, int *stop) {
    while (*s && !*stop) {
        if (*s == '\\' && s[1]) {
            s++;
            switch (*s) {
                case 'n': fputc('\n', stdout); break;
                case 't': fputc('\t', stdout); break;
                case 'r': fputc('\r', stdout); break;
                case 'a': fputc('\a', stdout); break;
                case 'b': fputc('\b', stdout); break;
                case 'f': fputc('\f', stdout); break;
                case 'v': fputc('\v', stdout); break;
                case '\\': fputc('\\', stdout); break;
                case 'c': *stop = 1; break;
                default: fputc('\\', stdout); fputc(*s, stdout); break;
            }
            s++;
        } else {
            fputc(*s, stdout);
            s++;
        }
    }
}

int main(int argc, char *argv[]) {
    int newline = 1;
    int escapes = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "neEh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'n': newline = 0; break;
            case 'e': escapes = 1; break;
            case 'E': escapes = 0; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int stop = 0;
    for (int a = optind; a < argc; a++) {
        if (a > optind) fputc(' ', stdout);
        if (escapes) output_escaped(argv[a], &stop);
        else fputs(argv[a], stdout);
        if (stop) break;
    }

    if (newline && !stop) fputc('\n', stdout);

    return 0;
}
