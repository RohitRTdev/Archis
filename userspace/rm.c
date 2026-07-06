#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: rm [-f] [-r] [-v] [-h] FILE...\n");
    printf("Remove (unlink) each FILE. Directories require -r.\n\n");
    printf("  -f, --force       ignore nonexistent files, never prompt\n");
    printf("  -r, -R, --recursive   remove directories and their contents recursively\n");
    printf("  -v, --verbose     print a message for each file removed\n");
    printf("  -h, --help        display this help and exit\n");
}

static void join_path(char *out, size_t out_size, const char *dir, const char *name) {
    size_t dlen = strlen(dir);
    int need_slash = (dlen > 0 && dir[dlen - 1] != '/');
    snprintf(out, out_size, need_slash ? "%s/%s" : "%s%s", dir, name);
}

static int remove_path(const char *path, int force, int recursive, int verbose);

static int remove_dir_contents(const char *path, int force, int recursive, int verbose) {
    handle_t h = sys_open("fs", path, 0);
    if (h < 0) return force ? 0 : 1;

    size_t offset = 0;
    char namebuf[PATH_MAX];
    int fail = 0;

    for (;;) {
        size_t written = 0;
        syscall_status_t rc = sys_readdir(h, offset, namebuf, sizeof(namebuf), &written);
        if (rc == E_NO_DIR_ENTRIES) break;
        if (rc != E_SUCCESS) {
            fail = 1;
            break;
        }
        offset++;

        if (strcmp(namebuf, ".") == 0 || strcmp(namebuf, "..") == 0) continue;

        char child[PATH_MAX];
        join_path(child, sizeof(child), path, namebuf);
        fail |= remove_path(child, force, recursive, verbose);
    }

    sys_close(h);
    return fail;
}

static int remove_path(const char *path, int force, int recursive, int verbose) {
    file_stat_t st;
    if (sys_stat(path, 0, &st) != E_SUCCESS) {
        if (force) return 0;
        fprintf(stderr, "rm: cannot remove '%s': No such file or directory\n", path);
        return 1;
    }

    int fail = 0;

    if (st.mode & FILE_MODE_DIR) {
        if (!recursive) {
            fprintf(stderr, "rm: cannot remove '%s': Is a directory\n", path);
            return 1;
        }
        fail |= remove_dir_contents(path, force, recursive, verbose);
    }

    if (sys_delete(path) != E_SUCCESS) {
        if (!force) {
            fprintf(stderr, "rm: cannot remove '%s'\n", path);
            fail = 1;
        }
    } else if (verbose) {
        printf("removed '%s'\n", path);
    }

    return fail;
}

int main(int argc, char *argv[]) {
    int force = 0, recursive = 0, verbose = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"force", no_argument, 0, 'f'},
        {"recursive", no_argument, 0, 'r'},
        {"verbose", no_argument, 0, 'v'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "frRvh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'f': force = 1; break;
            case 'r': case 'R': recursive = 1; break;
            case 'v': verbose = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (optind >= argc) {
        if (force) return 0;
        fprintf(stderr, "rm: missing operand\n");
        return 1;
    }

    int fail = 0;
    for (int i = optind; i < argc; i++) {
        fail |= remove_path(argv[i], force, recursive, verbose);
    }

    return fail ? 1 : 0;
}
