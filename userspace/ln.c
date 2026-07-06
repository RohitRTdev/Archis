#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: ln [-s] [-f] [-h] TARGET [LINK_NAME]\n");
    printf("       ln [-s] [-f] TARGET... DIRECTORY\n");
    printf("Create a symbolic link to TARGET, named LINK_NAME (or, with the\n");
    printf("second form, one link per TARGET inside DIRECTORY).\n\n");
    printf("There is no hardlink support on this system, so all links are symbolic.\n\n");
    printf("  -s, --symbolic   accepted for compatibility; links are always symbolic\n");
    printf("  -f, --force      remove an existing destination before linking\n");
    printf("  -h, --help       display this help and exit\n");
}

static const char *base_name(const char *path) {
    const char *slash = strrchr(path, '/');
    return slash ? slash + 1 : path;
}

static int is_directory(const char *path) {
    file_stat_t st;
    return sys_stat(path, STAT_FOLLOW_FLAG, &st) == E_SUCCESS && (st.mode & FILE_MODE_DIR);
}

static int make_link(const char *target, const char *linkname, int force) {
    if (force) sys_delete(linkname);
    if (sys_create_symlink(linkname, target) != E_SUCCESS) {
        fprintf(stderr, "ln: failed to create symbolic link '%s'\n", linkname);
        return 1;
    }
    return 0;
}

int main(int argc, char *argv[]) {
    int force = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"symbolic", no_argument, 0, 's'},
        {"force", no_argument, 0, 'f'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "sfh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 's': break;
            case 'f': force = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    int n = argc - optind;
    char **ops = argv + optind;

    if (n < 1) {
        fprintf(stderr, "ln: missing file operand\n");
        return 1;
    }

    if (n == 1) {
        return make_link(ops[0], base_name(ops[0]), force);
    }

    int last_is_dir = is_directory(ops[n - 1]);

    if (n == 2 && !last_is_dir) {
        return make_link(ops[0], ops[1], force);
    }

    if (!last_is_dir) {
        fprintf(stderr, "ln: target '%s' is not a directory\n", ops[n - 1]);
        return 1;
    }

    int fail = 0;
    for (int i = 0; i < n - 1; i++) {
        char linkname[PATH_MAX];
        size_t dlen = strlen(ops[n - 1]);
        int need_slash = (dlen > 0 && ops[n - 1][dlen - 1] != '/');
        snprintf(linkname, sizeof(linkname), need_slash ? "%s/%s" : "%s%s", ops[n - 1], base_name(ops[i]));
        fail |= make_link(ops[i], linkname, force);
    }

    return fail ? 1 : 0;
}
