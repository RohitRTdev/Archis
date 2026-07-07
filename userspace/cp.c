#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

typedef struct {
    int recursive;   // -r/-R
    int force;       // -f
    int interactive; // -i
    int no_clobber;  // -n
    int verbose;      // -v
} cp_opts_t;

static void print_help(void) {
    printf("Usage: cp [OPTION]... SOURCE DEST\n");
    printf("       cp [OPTION]... SOURCE... DIRECTORY\n");
    printf("Copy SOURCE to DEST, or multiple SOURCE(s) into DIRECTORY.\n\n");
    printf("  -f, --force         if an existing destination file cannot be opened,\n");
    printf("                        remove it and try again (implies not -i)\n");
    printf("  -i, --interactive   prompt before overwrite (implies not -f)\n");
    printf("  -n, --no-clobber    do not overwrite an existing file\n");
    printf("  -r, -R, --recursive copy directories recursively\n");
    printf("  -v, --verbose       explain what is being done\n");
    printf("  -h, --help          display this help and exit\n");
}

static void join_path(char *out, size_t out_size, const char *dir, const char *name) {
    size_t dlen = strlen(dir);
    int need_slash = (dlen > 0 && dir[dlen - 1] != '/');
    snprintf(out, out_size, need_slash ? "%s/%s" : "%s%s", dir, name);
}

static const char *base_name(const char *path) {
    const char *slash = strrchr(path, '/');
    return slash ? slash + 1 : path;
}

static int is_directory(const char *path) {
    file_stat_t st;
    return sys_stat(path, STAT_FOLLOW_FLAG, &st) == E_SUCCESS && (st.mode & FILE_MODE_DIR);
}

// A directory can't be copied into itself or into one of its own
// subdirectories -- there's no inode to compare here, so this is a plain
// path-prefix check rather than a canonical (symlink/`.`-resolved) one.
static int is_path_prefix(const char *base, const char *maybe_child) {
    size_t blen = strlen(base);
    if (strncmp(base, maybe_child, blen) != 0) return 0;
    return maybe_child[blen] == '/' || maybe_child[blen] == '\0';
}

static int should_overwrite(const cp_opts_t *opts, const char *dest, int dest_exists) {
    if (!dest_exists) return 1;
    if (opts->no_clobber) return 0;
    if (opts->interactive) {
        fprintf(stderr, "cp: overwrite '%s'? ", dest);
        char resp[16];
        if (!fgets(resp, sizeof(resp), stdin)) return 0;
        return resp[0] == 'y' || resp[0] == 'Y';
    }
    return 1;
}

static int copy_file_bytes(const char *src, const char *dest, const cp_opts_t *opts) {
    file_stat_t dst_st;
    int dest_exists = sys_stat(dest, 0, &dst_st) == E_SUCCESS;

    if (dest_exists && (dst_st.mode & FILE_MODE_DIR)) {
        fprintf(stderr, "cp: cannot overwrite directory '%s' with non-directory\n", dest);
        return 1;
    }

    if (!should_overwrite(opts, dest, dest_exists)) {
        if (opts->verbose) printf("skipped '%s'\n", dest);
        return 0;
    }

    FILE *in = fopen(src, "r");
    if (!in) {
        fprintf(stderr, "cp: cannot open '%s' for reading: No such file or directory\n", src);
        return 1;
    }

    FILE *out = fopen(dest, "w");
    if (!out && opts->force) {
        sys_delete(dest);
        out = fopen(dest, "w");
    }
    if (!out) {
        fprintf(stderr, "cp: cannot create regular file '%s'\n", dest);
        fclose(in);
        return 1;
    }

    char buf[4096];
    size_t n;
    int fail = 0;
    while ((n = fread(buf, 1, sizeof(buf), in)) > 0) {
        if (fwrite(buf, 1, n, out) != n) {
            fprintf(stderr, "cp: error writing '%s'\n", dest);
            fail = 1;
            break;
        }
    }
    if (!fail && ferror(in)) {
        fprintf(stderr, "cp: error reading '%s'\n", src);
        fail = 1;
    }

    fclose(in);
    fclose(out);

    if (!fail && opts->verbose) printf("'%s' -> '%s'\n", src, dest);
    return fail;
}

static int copy_path(const char *src, const char *dest, const cp_opts_t *opts);

static int copy_dir(const char *src, const char *dest, const cp_opts_t *opts) {
    if (is_path_prefix(src, dest)) {
        fprintf(stderr, "cp: cannot copy a directory, '%s', into itself, '%s'\n", src, dest);
        return 1;
    }

    file_stat_t dst_st;
    int dest_exists = sys_stat(dest, 0, &dst_st) == E_SUCCESS;
    if (dest_exists && !(dst_st.mode & FILE_MODE_DIR)) {
        fprintf(stderr, "cp: cannot overwrite non-directory '%s' with directory\n", dest);
        return 1;
    }

    if (!dest_exists) {
        if (sys_mkdir(dest) != E_SUCCESS) {
            fprintf(stderr, "cp: cannot create directory '%s'\n", dest);
            return 1;
        }
        if (opts->verbose) printf("'%s' -> '%s'\n", src, dest);
    }

    handle_t h = sys_open("fs", src, 0);
    if (h < 0) {
        fprintf(stderr, "cp: cannot access '%s': No such file or directory\n", src);
        return 1;
    }

    size_t offset = 0;
    char namebuf[PATH_MAX];
    int fail = 0;

    for (;;) {
        size_t written = 0;
        syscall_status_t rc = sys_readdir(h, offset, namebuf, sizeof(namebuf), &written);
        if (rc == E_NO_DIR_ENTRIES) break;
        if (rc != E_SUCCESS) {
            fprintf(stderr, "cp: error reading directory '%s'\n", src);
            fail = 1;
            break;
        }
        offset++;

        if (strcmp(namebuf, ".") == 0 || strcmp(namebuf, "..") == 0) continue;

        char child_src[PATH_MAX], child_dest[PATH_MAX];
        join_path(child_src, sizeof(child_src), src, namebuf);
        join_path(child_dest, sizeof(child_dest), dest, namebuf);
        fail |= copy_path(child_src, child_dest, opts);
    }

    sys_close(h);
    return fail;
}

static int copy_path(const char *src, const char *dest, const cp_opts_t *opts) {
    if (strcmp(src, dest) == 0) {
        fprintf(stderr, "cp: '%s' and '%s' are the same file\n", src, dest);
        return 1;
    }

    file_stat_t st;
    if (sys_stat(src, STAT_FOLLOW_FLAG, &st) != E_SUCCESS) {
        fprintf(stderr, "cp: cannot stat '%s': No such file or directory\n", src);
        return 1;
    }

    if (st.mode & FILE_MODE_DIR) {
        if (!opts->recursive) {
            fprintf(stderr, "cp: -r not specified; omitting directory '%s'\n", src);
            return 1;
        }
        return copy_dir(src, dest, opts);
    }

    return copy_file_bytes(src, dest, opts);
}

int main(int argc, char *argv[]) {
    cp_opts_t opts = {0};

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"force", no_argument, 0, 'f'},
        {"interactive", no_argument, 0, 'i'},
        {"no-clobber", no_argument, 0, 'n'},
        {"recursive", no_argument, 0, 'r'},
        {"verbose", no_argument, 0, 'v'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "finRrvh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'f': opts.force = 1; opts.interactive = 0; break;
            case 'i': opts.interactive = 1; opts.force = 0; break;
            case 'n': opts.no_clobber = 1; break;
            case 'r': case 'R': opts.recursive = 1; break;
            case 'v': opts.verbose = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int n = argc - optind;
    char **ops = argv + optind;

    if (n < 1) {
        fprintf(stderr, "cp: missing file operand\n");
        return 1;
    }
    if (n < 2) {
        fprintf(stderr, "cp: missing destination file operand after '%s'\n", ops[0]);
        return 1;
    }

    int last_is_dir = is_directory(ops[n - 1]);

    if (n == 2 && !last_is_dir) {
        return copy_path(ops[0], ops[1], &opts);
    }

    if (!last_is_dir) {
        fprintf(stderr, "cp: target '%s' is not a directory\n", ops[n - 1]);
        return 1;
    }

    int fail = 0;
    for (int i = 0; i < n - 1; i++) {
        char dest[PATH_MAX];
        join_path(dest, sizeof(dest), ops[n - 1], base_name(ops[i]));
        fail |= copy_path(ops[i], dest, &opts);
    }

    return fail ? 1 : 0;
}
