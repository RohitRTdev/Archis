#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

typedef struct {
    int show_all; // -a
    int long_fmt; // -l
} ls_opts_t;

static void print_help(void) {
    printf("Usage: ls [OPTION]... [FILE]...\n");
    printf("List information about FILEs (the current directory by default).\n\n");
    printf("  -a            do not ignore entries starting with .\n");
    printf("  -l            use a long listing format\n");
    printf("  -h, --help    display this help and exit\n");
}

static char type_char(uint16_t mode) {
    return (mode & FILE_MODE_DIR) ? 'd' : '-';
}

static void join_path(char *out, size_t out_size, const char *dir, const char *name) {
    size_t dlen = strlen(dir);
    int need_slash = (dlen > 0 && dir[dlen - 1] != '/');
    snprintf(out, out_size, need_slash ? "%s/%s" : "%s%s", dir, name);
}

static void print_symlink_line(const ls_opts_t *opts, const char *name, const char *path) {
    if (!opts->long_fmt) {
        printf("%s\n", name);
        return;
    }

    char target[PATH_MAX];
    size_t tlen = 0;
    sys_readlink(path, target, sizeof(target), &tlen);
    unsigned long long target_len = tlen > 0 ? (unsigned long long)(tlen - 1) : 0;
    printf("l %8llu %s -> %s\n", target_len, name, target);
}

static int list_dir_children(handle_t h, const char *path, const ls_opts_t *opts) {
    size_t offset = 0;
    char namebuf[PATH_MAX];
    int fail = 0;

    for (;;) {
        size_t written = 0;
        syscall_status_t rc = sys_readdir(h, offset, namebuf, sizeof(namebuf), &written);
        if (rc == E_NO_DIR_ENTRIES) break;
        if (rc != E_SUCCESS) {
            fprintf(stderr, "ls: error reading '%s'\n", path);
            fail = 1;
            break;
        }
        offset++;

        if (namebuf[0] == '.' && !opts->show_all) continue;

        if (!opts->long_fmt) {
            printf("%s\n", namebuf);
            continue;
        }

        char child_path[PATH_MAX];
        join_path(child_path, sizeof(child_path), path, namebuf);

        file_stat_t cst;
        if (sys_stat(child_path, 0, &cst) != E_SUCCESS) {
            printf("? %8llu %s\n", 0ULL, namebuf);
            continue;
        }

        if (cst.mode & FILE_MODE_SYMLINK) {
            print_symlink_line(opts, namebuf, child_path);
            continue;
        }

        printf("%c %8llu %s\n", type_char(cst.mode), (unsigned long long)cst.size, namebuf);
    }

    return fail;
}

static int list_one(const char *path, const ls_opts_t *opts) {
    file_stat_t st;
    if (sys_stat(path, 0, &st) != E_SUCCESS) {
        fprintf(stderr, "ls: cannot access '%s': No such file or directory\n", path);
        return 1;
    }

    int is_dir = (st.mode & FILE_MODE_DIR) != 0;

    if (st.mode & FILE_MODE_SYMLINK) {
        file_stat_t followed;
        is_dir = (sys_stat(path, STAT_FOLLOW_FLAG, &followed) == E_SUCCESS) && (followed.mode & FILE_MODE_DIR);

        if (!is_dir) {
            print_symlink_line(opts, path, path);
            return 0;
        }
    }

    if (!is_dir) {
        if (opts->long_fmt) {
            printf("%c %8llu %s\n", type_char(st.mode), (unsigned long long)st.size, path);
        } else {
            printf("%s\n", path);
        }
        return 0;
    }

    handle_t h = sys_open("fs", path, 0);
    if (h < 0) {
        fprintf(stderr, "ls: cannot access '%s': No such file or directory\n", path);
        return 1;
    }

    int fail = list_dir_children(h, path, opts);
    sys_close(h);
    return fail;
}

int main(int argc, char *argv[]) {
    ls_opts_t opts = {0};

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "alh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'a': opts.show_all = 1; break;
            case 'l': opts.long_fmt = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int fail = 0;

    if (optind >= argc) {
        fail |= list_one(".", &opts);
    } else {
        int multi = (argc - optind) > 1;
        int first = 1;
        for (int i = optind; i < argc; i++) {
            if (multi) {
                if (!first) printf("\n");
                printf("%s:\n", argv[i]);
            }
            first = 0;
            fail |= list_one(argv[i], &opts);
        }
    }

    return fail ? 1 : 0;
}
