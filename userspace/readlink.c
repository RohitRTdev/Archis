#include <stdio.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: readlink [-n] [-h] FILE...\n");
    printf("Print the value of each symbolic link FILE.\n\n");
    printf("  -n            do not output a trailing newline\n");
    printf("  -h, --help    display this help and exit\n");
}

int main(int argc, char *argv[]) {
    int no_newline = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "nh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'n': no_newline = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (optind >= argc) {
        fprintf(stderr, "readlink: missing operand\n");
        return 1;
    }

    int fail = 0;
    for (int i = optind; i < argc; i++) {
        char buf[PATH_MAX];
        size_t n = 0;
        if (sys_readlink(argv[i], buf, sizeof(buf), &n) != E_SUCCESS) {
            fail = 1;
            continue;
        }
        fputs(buf, stdout);
        if (!no_newline) fputc('\n', stdout);
    }

    return fail ? 1 : 0;
}
