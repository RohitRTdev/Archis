#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

typedef struct {
    int force;        // -f
    int interactive;  // -i
    int no_clobber;   // -n
    int verbose;       // -v
} mv_opts_t;

static void print_help(void) {
    printf("Usage: mv [OPTION]... SOURCE DEST\n");
    printf("       mv [OPTION]... SOURCE... DIRECTORY\n");
    printf("Rename SOURCE to DEST, or move SOURCE(s) into DIRECTORY.\n\n");
    printf("  -f, --force         do not prompt before overwriting (implies not -i)\n");
    printf("  -i, --interactive   prompt before overwrite (implies not -f)\n");
    printf("  -n, --no-clobber    do not overwrite an existing file\n");
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

// A directory can't be moved into itself or into one of its own
// subdirectories -- there's no inode to compare here, so this is a plain
// path-prefix check rather than a canonical (symlink/`.`-resolved) one.
static int is_path_prefix(const char *base, const char *maybe_child) {
    size_t blen = strlen(base);
    if (strncmp(base, maybe_child, blen) != 0) return 0;
    return maybe_child[blen] == '/' || maybe_child[blen] == '\0';
}

static int should_overwrite(const mv_opts_t *opts, const char *dest, int dest_exists) {
    if (!dest_exists) return 1;
    if (opts->no_clobber) return 0;
    if (opts->interactive) {
        fprintf(stderr, "mv: overwrite '%s'? ", dest);
        char resp[16];
        if (!fgets(resp, sizeof(resp), stdin)) return 0;
        return resp[0] == 'y' || resp[0] == 'Y';
    }
    return 1;
}

// Copies a single regular file's bytes (used only as the cross-mount rename
// fallback below -- the common case is handled by sys_rename_file directly).
static int copy_file_bytes(const char *src, const char *dest) {
    FILE *in = fopen(src, "r");
    if (!in) {
        fprintf(stderr, "mv: cannot open '%s' for reading: No such file or directory\n", src);
        return 1;
    }
    FILE *out = fopen(dest, "w");
    if (!out) {
        fprintf(stderr, "mv: cannot create regular file '%s'\n", dest);
        fclose(in);
        return 1;
    }

    char buf[4096];
    size_t n;
    int fail = 0;
    while ((n = fread(buf, 1, sizeof(buf), in)) > 0) {
        if (fwrite(buf, 1, n, out) != n) {
            fprintf(stderr, "mv: error writing '%s'\n", dest);
            fail = 1;
            break;
        }
    }
    if (!fail && ferror(in)) {
        fprintf(stderr, "mv: error reading '%s'\n", src);
        fail = 1;
    }

    fclose(in);
    fclose(out);
    return fail;
}

static int move_path(const char *src, const char *dest, const mv_opts_t *opts);

// Fallback for when sys_rename_file can't relink the entry in place (e.g. the
// two paths resolve to different mounted filesystems): recursively copy src
// to dest, then remove src, mirroring what a real mv does on EXDEV.
static int copy_then_remove(const char *src, const char *dest, int src_is_dir) {
    if (!src_is_dir) {
        if (copy_file_bytes(src, dest) != 0) return 1;
        if (sys_delete(src) != E_SUCCESS) {
            fprintf(stderr, "mv: cannot remove '%s' after copying\n", src);
            return 1;
        }
        return 0;
    }

    if (sys_mkdir(dest) != E_SUCCESS) {
        fprintf(stderr, "mv: cannot create directory '%s'\n", dest);
        return 1;
    }

    handle_t h = sys_open("fs", src, 0);
    if (h < 0) {
        fprintf(stderr, "mv: cannot access '%s': No such file or directory\n", src);
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
            fprintf(stderr, "mv: error reading directory '%s'\n", src);
            fail = 1;
            break;
        }
        offset++;

        if (strcmp(namebuf, ".") == 0 || strcmp(namebuf, "..") == 0) continue;

        char child_src[PATH_MAX], child_dest[PATH_MAX];
        join_path(child_src, sizeof(child_src), src, namebuf);
        join_path(child_dest, sizeof(child_dest), dest, namebuf);
        fail |= move_path(child_src, child_dest, NULL);
    }

    sys_close(h);
    if (fail) return 1;

    if (sys_delete(src) != E_SUCCESS) {
        fprintf(stderr, "mv: cannot remove '%s'\n", src);
        return 1;
    }
    return 0;
}

// Moves one entry. `opts` may be NULL when called for a child during a
// recursive copy_then_remove fallback -- children always move unconditionally
// (force semantics), since the overwrite prompt/no-clobber decision was
// already made for the tree as a whole at the top level.
static int move_path(const char *src, const char *dest, const mv_opts_t *opts) {
    if (strcmp(src, dest) == 0) {
        fprintf(stderr, "mv: '%s' and '%s' are the same file\n", src, dest);
        return 1;
    }

    if (is_path_prefix(src, dest)) {
        fprintf(stderr, "mv: cannot move '%s' to a subdirectory of itself, '%s'\n", src, dest);
        return 1;
    }

    file_stat_t src_st;
    if (sys_stat(src, 0, &src_st) != E_SUCCESS) {
        fprintf(stderr, "mv: cannot stat '%s': No such file or directory\n", src);
        return 1;
    }
    int src_is_dir = (src_st.mode & FILE_MODE_DIR) != 0;

    file_stat_t dst_st;
    int dest_exists = sys_stat(dest, 0, &dst_st) == E_SUCCESS;

    if (dest_exists) {
        int dest_is_dir = (dst_st.mode & FILE_MODE_DIR) != 0;
        if (src_is_dir && !dest_is_dir) {
            fprintf(stderr, "mv: cannot overwrite non-directory '%s' with directory\n", dest);
            return 1;
        }
        if (!src_is_dir && dest_is_dir) {
            fprintf(stderr, "mv: cannot overwrite directory '%s' with non-directory\n", dest);
            return 1;
        }

        if (opts && !should_overwrite(opts, dest, dest_exists)) {
            if (opts->verbose) printf("skipped '%s'\n", dest);
            return 0;
        }

        // sys_rename_file always fails if `dest` already exists (it never
        // overwrites), so the slot has to be cleared first. A non-empty
        // directory destination correctly fails here with "not empty".
        if (sys_delete(dest) != E_SUCCESS) {
            fprintf(stderr, "mv: cannot overwrite '%s'\n", dest);
            return 1;
        }
    }

    if (sys_rename_file(src, dest) == E_SUCCESS) {
        if (opts && opts->verbose) printf("'%s' -> '%s'\n", src, dest);
        return 0;
    }

    return copy_then_remove(src, dest, src_is_dir);
}

int main(int argc, char *argv[]) {
    mv_opts_t opts = {0};

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"force", no_argument, 0, 'f'},
        {"interactive", no_argument, 0, 'i'},
        {"no-clobber", no_argument, 0, 'n'},
        {"verbose", no_argument, 0, 'v'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "finvh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'f': opts.force = 1; opts.interactive = 0; break;
            case 'i': opts.interactive = 1; opts.force = 0; break;
            case 'n': opts.no_clobber = 1; break;
            case 'v': opts.verbose = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int n = argc - optind;
    char **ops = argv + optind;

    if (n < 1) {
        fprintf(stderr, "mv: missing file operand\n");
        return 1;
    }
    if (n < 2) {
        fprintf(stderr, "mv: missing destination file operand after '%s'\n", ops[0]);
        return 1;
    }

    int last_is_dir = is_directory(ops[n - 1]);

    if (n == 2 && !last_is_dir) {
        return move_path(ops[0], ops[1], &opts);
    }

    if (!last_is_dir) {
        fprintf(stderr, "mv: target '%s' is not a directory\n", ops[n - 1]);
        return 1;
    }

    int fail = 0;
    for (int i = 0; i < n - 1; i++) {
        char dest[PATH_MAX];
        join_path(dest, sizeof(dest), ops[n - 1], base_name(ops[i]));
        fail |= move_path(ops[i], dest, &opts);
    }

    return fail ? 1 : 0;
}
