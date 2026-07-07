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
    int from_start; // count starts with '+': start printing at absolute line/byte count
    int quiet;
    int verbose;
} tail_opts_t;

static void print_help(void) {
    printf("Usage: tail [OPTION]... [FILE]...\n");
    printf("Print the last 10 lines of each FILE to standard output.\n");
    printf("With more than one FILE, precede each with a header giving the file name.\n\n");
    printf("With no FILE, or when FILE is -, read standard input.\n\n");
    printf("  -c, --bytes=[+]NUM     print the last NUM bytes; with a leading '+',\n");
    printf("                           print starting with byte NUM of each file\n");
    printf("  -n, --lines=[+]NUM     print the last NUM lines instead of the last 10;\n");
    printf("                           with a leading '+', print starting with line NUM\n");
    printf("  -q, --quiet, --silent  never print headers giving file names\n");
    printf("  -v, --verbose          always print headers giving file names\n");
    printf("  -h, --help             display this help and exit\n");
}

static int parse_count(const char *s, long long *out, int *from_start) {
    *from_start = (*s == '+');
    if (*s == '+' || *s == '-') s++;
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

typedef struct {
    char *buf;
    size_t len;
    int nl;
} pending_line_t;

static int tail_last_lines(FILE *f, long long n) {
    if (n == 0) return ferror(f) ? 1 : 0;

    size_t cap = (size_t)n;
    pending_line_t *queue = calloc(cap, sizeof(pending_line_t));
    if (!queue) { fprintf(stderr, "tail: out of memory\n"); return 1; }

    size_t count = 0, head_idx = 0;
    int fail = 0;

    for (;;) {
        char *line; size_t len; int nl;
        int r = read_line(f, &line, &len, &nl);
        if (r == 0) break;
        if (r < 0) { fprintf(stderr, "tail: out of memory\n"); fail = 1; break; }

        if (count == cap) {
            free(queue[head_idx].buf);
            queue[head_idx].buf = line;
            queue[head_idx].len = len;
            queue[head_idx].nl = nl;
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
        pending_line_t *pl = &queue[(head_idx + i) % cap];
        fwrite(pl->buf, 1, pl->len, stdout);
        if (pl->nl) fputc('\n', stdout);
        free(pl->buf);
    }
    free(queue);

    if (ferror(f)) fail = 1;
    return fail;
}

static int tail_last_bytes(FILE *f, long long n) {
    if (n == 0) return ferror(f) ? 1 : 0;

    size_t cap = (size_t)n;
    unsigned char *ring = malloc(cap);
    if (!ring) { fprintf(stderr, "tail: out of memory\n"); return 1; }

    size_t filled = 0, pos = 0;
    int c;
    while ((c = fgetc(f)) != EOF) {
        ring[pos] = (unsigned char)c;
        pos = (pos + 1) % cap;
        if (filled < cap) filled++;
    }

    size_t start = (filled < cap) ? 0 : pos;
    for (size_t i = 0; i < filled; i++) {
        fputc(ring[(start + i) % cap], stdout);
    }

    free(ring);
    return ferror(f) ? 1 : 0;
}

static int tail_from_line(FILE *f, long long start) {
    if (start < 1) start = 1;

    long long current_line = 1;
    int c;
    while ((c = fgetc(f)) != EOF) {
        if (current_line >= start) fputc(c, stdout);
        if (c == '\n') current_line++;
    }
    return ferror(f) ? 1 : 0;
}

static int tail_from_byte(FILE *f, long long start) {
    if (start < 1) start = 1;

    long long idx = 1;
    int c;
    while ((c = fgetc(f)) != EOF) {
        if (idx >= start) fputc(c, stdout);
        idx++;
    }
    return ferror(f) ? 1 : 0;
}

static int tail_stream(FILE *f, const tail_opts_t *opts) {
    if (opts->mode == MODE_LINES) {
        return opts->from_start ? tail_from_line(f, opts->count) : tail_last_lines(f, opts->count);
    }
    return opts->from_start ? tail_from_byte(f, opts->count) : tail_last_bytes(f, opts->count);
}

int main(int argc, char *argv[]) {
    tail_opts_t opts;
    opts.mode = MODE_LINES;
    opts.count = 10;
    opts.from_start = 0;
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
                if (parse_count(optarg, &opts.count, &opts.from_start) < 0) {
                    fprintf(stderr, "tail: invalid number of bytes: '%s'\n", optarg);
                    return 1;
                }
                opts.mode = MODE_BYTES;
                break;
            case 'n':
                if (parse_count(optarg, &opts.count, &opts.from_start) < 0) {
                    fprintf(stderr, "tail: invalid number of lines: '%s'\n", optarg);
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
        fail |= tail_stream(stdin, &opts);
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
                    fprintf(stderr, "tail: error reading '%s': Is a directory\n", path);
                    fail = 1;
                    continue;
                }
            }

            FILE *f = is_stdin ? stdin : fopen(path, "r");
            if (!f) {
                fprintf(stderr, "tail: cannot open '%s' for reading: No such file or directory\n", path);
                fail = 1;
                continue;
            }

            if (print_headers) {
                if (!first) printf("\n");
                printf("==> %s <==\n", is_stdin ? "standard input" : path);
            }
            first = 0;

            fail |= tail_stream(f, &opts);
            if (!is_stdin) fclose(f);
        }
    }

    return fail ? 1 : 0;
}
