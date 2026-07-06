#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

typedef struct {
    int number_lines;    // -n
    int number_nonblank; // -b (overrides -n)
    int squeeze_blank;   // -s
    int show_ends;       // -E / -A
} cat_opts_t;

static void print_help(void) {
    printf("Usage: cat [OPTION]... [FILE]...\n");
    printf("Concatenate FILE(s) to standard output.\n\n");
    printf("With no FILE, or when FILE is -, read standard input.\n\n");
    printf("  -A            equivalent to -E\n");
    printf("  -b            number nonempty output lines\n");
    printf("  -E            display $ at end of each line\n");
    printf("  -n            number all output lines\n");
    printf("  -s            suppress repeated empty output lines\n");
    printf("  -h, --help    display this help and exit\n");
}

// Reads one line (without the newline) into a malloc'd, NUL-terminated buffer.
// Returns 1 with *out_buf/*out_len/*got_newline set, 0 on clean EOF (nothing read),
// -1 on allocation failure.
static int read_line(FILE *f, char **out_buf, size_t *out_len, int *got_newline) {
    size_t cap = 128, len = 0;
    char *buf = malloc(cap);
    if (!buf) return -1;

    int c;
    int any = 0;
    *got_newline = 0;

    while ((c = fgetc(f)) != EOF) {
        any = 1;
        if (c == '\n') { *got_newline = 1; break; }
        if (len + 1 >= cap) {
            cap *= 2;
            char *nb = realloc(buf, cap);
            if (!nb) { free(buf); return -1; }
            buf = nb;
        }
        buf[len++] = (char)c;
    }

    if (!any) { free(buf); return 0; }

    buf[len] = '\0';
    *out_buf = buf;
    *out_len = len;
    return 1;
}

static int cat_stream(FILE *f, const cat_opts_t *opts, long long *lineno, int *prev_blank) {
    int had_error = 0;

    for (;;) {
        char *line;
        size_t len;
        int nl;
        int r = read_line(f, &line, &len, &nl);
        if (r == 0) break;
        if (r < 0) { fprintf(stderr, "cat: out of memory\n"); return 1; }

        int is_blank = (len == 0);
        if (opts->squeeze_blank && is_blank && *prev_blank) {
            free(line);
            continue;
        }

        int do_number = opts->number_nonblank ? !is_blank : opts->number_lines;
        if (do_number) printf("%6lld\t", (*lineno)++);

        fwrite(line, 1, len, stdout);
        if (opts->show_ends) fputc('$', stdout);
        if (nl) fputc('\n', stdout);

        *prev_blank = is_blank;
        free(line);
    }

    if (ferror(f)) had_error = 1;
    return had_error;
}

int main(int argc, char *argv[]) {
    cat_opts_t opts = {0};

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "nbsEAh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'n': opts.number_lines = 1; break;
            case 'b': opts.number_nonblank = 1; break;
            case 's': opts.squeeze_blank = 1; break;
            case 'E': case 'A': opts.show_ends = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int fail = 0;
    long long lineno = 1;
    int prev_blank = 0;

    if (optind >= argc) {
        fail |= cat_stream(stdin, &opts, &lineno, &prev_blank);
    } else {
        for (int i = optind; i < argc; i++) {
            const char *path = argv[i];
            int is_stdin = strcmp(path, "-") == 0;

            if (!is_stdin) {
                file_stat_t st;
                if (sys_stat(path, STAT_FOLLOW_FLAG, &st) == E_SUCCESS && (st.mode & FILE_MODE_DIR)) {
                    fprintf(stderr, "cat: %s: Is a directory\n", path);
                    fail = 1;
                    continue;
                }
            }

            FILE *f = is_stdin ? stdin : fopen(path, "r");
            if (!f) {
                fprintf(stderr, "cat: %s: No such file or directory\n", path);
                fail = 1;
                continue;
            }

            fail |= cat_stream(f, &opts, &lineno, &prev_blank);
            if (!is_stdin) fclose(f);
        }
    }

    return fail ? 1 : 0;
}
