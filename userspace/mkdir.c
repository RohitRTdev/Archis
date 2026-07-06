#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: mkdir [-p] [-v] [-h] DIRECTORY...\n");
    printf("Create the DIRECTORY(ies), if they do not already exist.\n\n");
    printf("  -p, --parents   make parent directories as needed, no error if existing\n");
    printf("  -v, --verbose   print a message for each created directory\n");
    printf("  -h, --help      display this help and exit\n");
}

static int mkdir_p(const char *path, int verbose) {
    char buf[PATH_MAX];
    strncpy(buf, path, sizeof(buf) - 1);
    buf[sizeof(buf) - 1] = '\0';

    size_t len = strlen(buf);
    while (len > 1 && buf[len - 1] == '/') buf[--len] = '\0';

    for (size_t i = 1; i < len; i++) {
        if (buf[i] != '/') continue;

        buf[i] = '\0';
        syscall_status_t rc = sys_mkdir(buf);
        if (rc != E_SUCCESS && rc != E_FILE_EXISTS) {
            fprintf(stderr, "mkdir: cannot create directory '%s'\n", buf);
            return 1;
        }
        if (verbose && rc == E_SUCCESS) printf("mkdir: created directory '%s'\n", buf);
        buf[i] = '/';
    }

    syscall_status_t rc = sys_mkdir(buf);
    if (rc != E_SUCCESS && rc != E_FILE_EXISTS) {
        fprintf(stderr, "mkdir: cannot create directory '%s'\n", buf);
        return 1;
    }
    if (verbose && rc == E_SUCCESS) printf("mkdir: created directory '%s'\n", buf);

    return 0;
}

int main(int argc, char *argv[]) {
    int parents = 0, verbose = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"parents", no_argument, 0, 'p'},
        {"verbose", no_argument, 0, 'v'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "pvh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'p': parents = 1; break;
            case 'v': verbose = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (optind >= argc) {
        fprintf(stderr, "mkdir: missing operand\n");
        return 1;
    }

    int fail = 0;
    for (int i = optind; i < argc; i++) {
        if (parents) {
            fail |= mkdir_p(argv[i], verbose);
            continue;
        }

        if (sys_mkdir(argv[i]) != E_SUCCESS) {
            fprintf(stderr, "mkdir: cannot create directory '%s'\n", argv[i]);
            fail = 1;
            continue;
        }
        if (verbose) printf("mkdir: created directory '%s'\n", argv[i]);
    }

    return fail ? 1 : 0;
}
