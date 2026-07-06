#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: rmdir [-p] [-h] DIRECTORY...\n");
    printf("Remove the DIRECTORY(ies), if they are empty.\n\n");
    printf("  -p, --parents   remove DIRECTORY and its ancestors, as long as they become empty\n");
    printf("  -h, --help      display this help and exit\n");
}

// Strips the trailing path component from `path` in place (e.g. "/a/b/c" -> "/a/b").
static void strip_last_component(char *path) {
    size_t len = strlen(path);
    while (len > 1 && path[len - 1] == '/') path[--len] = '\0';

    char *slash = strrchr(path, '/');
    if (!slash) { path[0] = '\0'; return; }
    if (slash == path) { path[1] = '\0'; return; }
    *slash = '\0';
}

static int rmdir_one(const char *path, int parents) {
    file_stat_t st;
    if (sys_stat(path, STAT_FOLLOW_FLAG, &st) != E_SUCCESS || !(st.mode & FILE_MODE_DIR)) {
        fprintf(stderr, "rmdir: failed to remove '%s': Not a directory\n", path);
        return 1;
    }

    if (sys_delete(path) != E_SUCCESS) {
        fprintf(stderr, "rmdir: failed to remove '%s': directory not empty\n", path);
        return 1;
    }

    if (parents) {
        char buf[PATH_MAX];
        strncpy(buf, path, sizeof(buf) - 1);
        buf[sizeof(buf) - 1] = '\0';

        for (;;) {
            strip_last_component(buf);
            if (buf[0] == '\0' || strcmp(buf, "/") == 0 || strcmp(buf, ".") == 0) break;
            if (sys_delete(buf) != E_SUCCESS) break;
        }
    }

    return 0;
}

int main(int argc, char *argv[]) {
    int parents = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"parents", no_argument, 0, 'p'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "ph", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'p': parents = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (optind >= argc) {
        fprintf(stderr, "rmdir: missing operand\n");
        return 1;
    }

    int fail = 0;
    for (int i = optind; i < argc; i++) {
        fail |= rmdir_one(argv[i], parents);
    }

    return fail ? 1 : 0;
}
