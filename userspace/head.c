#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

typedef enum { MODE_LINES, MODE_BYTES } count_mode_t;

typedef struct {
    count_mode_t mode;
    long long count;
    int exclude_from_end; // count starts with '-': print all but the last count lines/bytes
    int quiet;
    int verbose;
} head_opts_t;

static void print_help(void) {
    printf("Usage: head [OPTION]... [FILE]...\n");
    printf("Print the first 10 lines of each FILE to standard output.\n");
    printf("With more than one FILE, precede each with a header giving the file name.\n\n");
    printf("With no FILE, or when FILE is -, read standard input.\n\n");
    printf("  -c, --bytes=[-]NUM     print the first NUM bytes of each file;\n");
    printf("                           with a leading '-', print all but the last NUM bytes\n");
    printf("  -n, --lines=[-]NUM     print the first NUM lines instead of the first 10;\n");
    printf("                           with a leading '-', print all but the last NUM lines\n");
    printf("  -q, --quiet, --silent  never print headers giving file names\n");
    printf("  -v, --verbose          always print headers giving file names\n");
    printf("  -h, --help             display this help and exit\n");
}

static int parse_count(const char *s, long long *out, int *exclude) {
    *exclude = (*s == '-');
    if (*exclude) s++;
    char *end;
    long long v = strtoll(s, &end, 10);
    if (*end != '\0' || v < 0) return -1;
    *out = v;
    return 0;
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

static int head_first_lines(FILE *f, long long n) {
    if (n == 0) return ferror(f) ? 1 : 0;

    long long lines = 0;
    int c;
    while ((c = fgetc(f)) != EOF) {
        fputc(c, stdout);
        if (c == '\n' && ++lines == n) break;
    }
    return ferror(f) ? 1 : 0;
}

static int head_first_bytes(FILE *f, long long n) {
    long long i = 0;
    int c;
    while (i < n && (c = fgetc(f)) != EOF) {
        fputc(c, stdout);
        i++;
    }
    return ferror(f) ? 1 : 0;
}

typedef struct {
    char *buf;
    size_t len;
    int nl;
} pending_line_t;

static int head_all_but_last_lines(FILE *f, long long n) {
    if (n <= 0) {
        int c;
        while ((c = fgetc(f)) != EOF) fputc(c, stdout);
        return ferror(f) ? 1 : 0;
    }

    size_t cap = (size_t)n;
    pending_line_t *queue = calloc(cap, sizeof(pending_line_t));
    if (!queue) { fprintf(stderr, "head: out of memory\n"); return 1; }

    size_t count = 0, head_idx = 0;
    int fail = 0;

    for (;;) {
        char *line; size_t len; int nl;
        int r = read_line(f, &line, &len, &nl);
        if (r == 0) break;
        if (r < 0) { fprintf(stderr, "head: out of memory\n"); fail = 1; break; }

        if (count == cap) {
            pending_line_t *old = &queue[head_idx];
            fwrite(old->buf, 1, old->len, stdout);
            if (old->nl) fputc('\n', stdout);
            free(old->buf);
            old->buf = line;
            old->len = len;
            old->nl = nl;
            head_idx = (head_idx + 1) % cap;
        } else {
            size_t slot = (head_idx + count) % cap;
            queue[slot].buf = line;
            queue[slot].len = len;
            queue[slot].nl = nl;
            count++;
        }
    }

    for (size_t i = 0; i < count; i++) {
        free(queue[(head_idx + i) % cap].buf);
    }
    free(queue);

    if (ferror(f)) fail = 1;
    return fail;
}

static int head_all_but_last_bytes(FILE *f, long long n) {
    if (n <= 0) {
        int c;
        while ((c = fgetc(f)) != EOF) fputc(c, stdout);
        return ferror(f) ? 1 : 0;
    }

    size_t cap = (size_t)n;
    unsigned char *ring = malloc(cap);
    if (!ring) { fprintf(stderr, "head: out of memory\n"); return 1; }

    size_t filled = 0, pos = 0;
    int c;
    while ((c = fgetc(f)) != EOF) {
        if (filled == cap) {
            fputc(ring[pos], stdout);
            ring[pos] = (unsigned char)c;
            pos = (pos + 1) % cap;
        } else {
            ring[(pos + filled) % cap] = (unsigned char)c;
            filled++;
        }
    }

    free(ring);
    return ferror(f) ? 1 : 0;
}

static int head_stream(FILE *f, const head_opts_t *opts) {
    if (opts->mode == MODE_LINES) {
        return opts->exclude_from_end ? head_all_but_last_lines(f, opts->count)
                                       : head_first_lines(f, opts->count);
    }
    return opts->exclude_from_end ? head_all_but_last_bytes(f, opts->count)
                                   : head_first_bytes(f, opts->count);
}

int main(int argc, char *argv[]) {
    head_opts_t opts;
    opts.mode = MODE_LINES;
    opts.count = 10;
    opts.exclude_from_end = 0;
    opts.quiet = 0;
    opts.verbose = 0;

    static struct option long_opts[] = {
        {"bytes", required_argument, 0, 'c'},
        {"lines", required_argument, 0, 'n'},
        {"quiet", no_argument, 0, 'q'},
        {"silent", no_argument, 0, 'q'},
        {"verbose", no_argument, 0, 'v'},
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "c:n:qvh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'c':
                if (parse_count(optarg, &opts.count, &opts.exclude_from_end) < 0) {
                    fprintf(stderr, "head: invalid number of bytes: '%s'\n", optarg);
                    return 1;
                }
                opts.mode = MODE_BYTES;
                break;
            case 'n':
                if (parse_count(optarg, &opts.count, &opts.exclude_from_end) < 0) {
                    fprintf(stderr, "head: invalid number of lines: '%s'\n", optarg);
                    return 1;
                }
                opts.mode = MODE_LINES;
                break;
            case 'q': opts.quiet = 1; break;
            case 'v': opts.verbose = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int fail = 0;

    if (optind >= argc) {
        fail |= head_stream(stdin, &opts);
    } else {
        int multi = (argc - optind) > 1;
        int print_headers = opts.verbose || (multi && !opts.quiet);
        int first = 1;

        for (int i = optind; i < argc; i++) {
            const char *path = argv[i];
            int is_stdin = strcmp(path, "-") == 0;

            if (!is_stdin) {
                file_stat_t st;
                if (sys_stat(path, STAT_FOLLOW_FLAG, &st) == E_SUCCESS && (st.mode & FILE_MODE_DIR)) {
                    fprintf(stderr, "head: error reading '%s': Is a directory\n", path);
                    fail = 1;
                    continue;
                }
            }

            FILE *f = is_stdin ? stdin : fopen(path, "r");
            if (!f) {
                fprintf(stderr, "head: cannot open '%s' for reading: No such file or directory\n", path);
                fail = 1;
                continue;
            }

            if (print_headers) {
                if (!first) printf("\n");
                printf("==> %s <==\n", is_stdin ? "standard input" : path);
            }
            first = 0;

            fail |= head_stream(f, &opts);
            if (!is_stdin) fclose(f);
        }
    }

    return fail ? 1 : 0;
}
