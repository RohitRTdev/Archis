#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <ctype.h>
#include <sys/syscall.h>

typedef struct {
    int lines;
    int words;
    int chars;
    int bytes;
    int max_line_length;
} wc_opts_t;

typedef struct {
    unsigned long long lines;
    unsigned long long words;
    unsigned long long chars;
    unsigned long long bytes;
    unsigned long long max_line_length;
    char name[PATH_MAX];
    int has_name;
} wc_result_t;

static void print_help(void) {
    printf("Usage: wc [OPTION]... [FILE]...\n");
    printf("Print newline, word, and byte counts for each FILE, and a total line if\n");
    printf("more than one FILE is specified.\n\n");
    printf("With no FILE, or when FILE is -, read standard input.\n\n");
    printf("  -c, --bytes             print the byte counts\n");
    printf("  -m, --chars             print the character counts\n");
    printf("  -l, --lines             print the newline counts\n");
    printf("  -L, --max-line-length   print the maximum display width\n");
    printf("  -w, --words             print the word counts\n");
    printf("  -h, --help              display this help and exit\n");
}

static int wc_count(FILE *f, wc_result_t *r) {
    int in_word = 0;
    unsigned long long line_len = 0;
    int c;

    while ((c = fgetc(f)) != EOF) {
        r->bytes++;
        r->chars++;

        if (isspace(c)) {
            in_word = 0;
        } else if (!in_word) {
            in_word = 1;
            r->words++;
        }

        if (c == '\n') {
            r->lines++;
            if (line_len > r->max_line_length) r->max_line_length = line_len;
            line_len = 0;
        } else {
            line_len++;
        }
    }

    if (line_len > r->max_line_length) r->max_line_length = line_len;

    return ferror(f) ? 1 : 0;
}

static int count_digits(unsigned long long v) {
    int digits = 1;
    while (v >= 10) { v /= 10; digits++; }
    return digits;
}

static void print_row(const wc_opts_t *opts, const wc_result_t *r, int width) {
    char fmt[16];
    snprintf(fmt, sizeof(fmt), "%%%dllu", width);

    int printed = 0;
    if (opts->lines) { if (printed) fputc(' ', stdout); printf(fmt, r->lines); printed = 1; }
    if (opts->words) { if (printed) fputc(' ', stdout); printf(fmt, r->words); printed = 1; }
    if (opts->chars) { if (printed) fputc(' ', stdout); printf(fmt, r->chars); printed = 1; }
    if (opts->bytes) { if (printed) fputc(' ', stdout); printf(fmt, r->bytes); printed = 1; }
    if (opts->max_line_length) { if (printed) fputc(' ', stdout); printf(fmt, r->max_line_length); printed = 1; }

    if (r->has_name) printf(" %s", r->name);
    printf("\n");
}

int main(int argc, char *argv[]) {
    wc_opts_t opts = {0};

    static struct option long_opts[] = {
        {"bytes", no_argument, 0, 'c'},
        {"chars", no_argument, 0, 'm'},
        {"lines", no_argument, 0, 'l'},
        {"max-line-length", no_argument, 0, 'L'},
        {"words", no_argument, 0, 'w'},
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "cmlLwh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'c': opts.bytes = 1; break;
            case 'm': opts.chars = 1; break;
            case 'l': opts.lines = 1; break;
            case 'L': opts.max_line_length = 1; break;
            case 'w': opts.words = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (!(opts.lines || opts.words || opts.chars || opts.bytes || opts.max_line_length)) {
        opts.lines = opts.words = opts.bytes = 1;
    }

    int fail = 0;
    size_t cap = 8, count = 0;
    wc_result_t *results = malloc(cap * sizeof(*results));
    if (!results) { fprintf(stderr, "wc: out of memory\n"); return 1; }

    if (optind >= argc) {
        wc_result_t r = {0};
        fail |= wc_count(stdin, &r);
        results[count++] = r;
    } else {
        for (int i = optind; i < argc; i++) {
            const char *path = argv[i];
            int is_stdin = strcmp(path, "-") == 0;

            if (!is_stdin) {
                file_stat_t st;
                if (sys_stat(path, STAT_FOLLOW_FLAG, &st) == E_SUCCESS && (st.mode & FILE_MODE_DIR)) {
                    fprintf(stderr, "wc: %s: Is a directory\n", path);
                    fail = 1;
                    continue;
                }
            }

            FILE *f = is_stdin ? stdin : fopen(path, "r");
            if (!f) {
                fprintf(stderr, "wc: %s: No such file or directory\n", path);
                fail = 1;
                continue;
            }

            wc_result_t r = {0};
            fail |= wc_count(f, &r);
            snprintf(r.name, sizeof(r.name), "%s", path);
            r.has_name = 1;

            if (count == cap) {
                cap *= 2;
                wc_result_t *nr = realloc(results, cap * sizeof(*results));
                if (!nr) { fprintf(stderr, "wc: out of memory\n"); fail = 1; break; }
                results = nr;
            }
            results[count++] = r;

            if (!is_stdin) fclose(f);
        }
    }

    wc_result_t total = {0};
    for (size_t i = 0; i < count; i++) {
        total.lines += results[i].lines;
        total.words += results[i].words;
        total.chars += results[i].chars;
        total.bytes += results[i].bytes;
        if (results[i].max_line_length > total.max_line_length) {
            total.max_line_length = results[i].max_line_length;
        }
    }
    int show_total = count > 1;
    if (show_total) {
        snprintf(total.name, sizeof(total.name), "total");
        total.has_name = 1;
    }

    unsigned long long widest = 1;
    for (size_t i = 0; i < count; i++) {
        if (opts.lines && results[i].lines > widest) widest = results[i].lines;
        if (opts.words && results[i].words > widest) widest = results[i].words;
        if (opts.chars && results[i].chars > widest) widest = results[i].chars;
        if (opts.bytes && results[i].bytes > widest) widest = results[i].bytes;
        if (opts.max_line_length && results[i].max_line_length > widest) widest = results[i].max_line_length;
    }
    if (show_total) {
        if (opts.lines && total.lines > widest) widest = total.lines;
        if (opts.words && total.words > widest) widest = total.words;
        if (opts.chars && total.chars > widest) widest = total.chars;
        if (opts.bytes && total.bytes > widest) widest = total.bytes;
        if (opts.max_line_length && total.max_line_length > widest) widest = total.max_line_length;
    }
    int width = count_digits(widest);

    for (size_t i = 0; i < count; i++) {
        print_row(&opts, &results[i], width);
    }
    if (show_total) print_row(&opts, &total, width);

    free(results);
    return fail ? 1 : 0;
}
