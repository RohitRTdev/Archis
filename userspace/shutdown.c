#include <stdio.h>
#include <string.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: shutdown [OPTION]... [now]\n");
    printf("Halt, power off, or reboot the machine immediately.\n\n");
    printf("  -h            halt/power off (default)\n");
    printf("  -P            power off (same as -h)\n");
    printf("      --halt        halt/power off (same as -h)\n");
    printf("      --poweroff    power off (same as -h)\n");
    printf("  -r, --reboot  reboot the machine\n");
    printf("      --help        display this help and exit\n\n");
    printf("Only immediate shutdown is supported: no delay, wall message, or -c cancel.\n");
}

#define OPT_HELP 1000

int main(int argc, char *argv[]) {
    int restart = 0;

    static struct option long_opts[] = {
        {"halt",     no_argument, 0, 'h'},
        {"poweroff", no_argument, 0, 'P'},
        {"reboot",   no_argument, 0, 'r'},
        {"help",     no_argument, 0, OPT_HELP},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "hPr", long_opts, NULL)) != -1) {
        switch (opt) {
            case 'h': case 'P': restart = 0; break;
            case 'r': restart = 1; break;
            case OPT_HELP: print_help(); return 0;
            default: return 1;
        }
    }

    if (optind < argc) {
        if (strcmp(argv[optind], "now") != 0 || optind + 1 < argc) {
            fprintf(stderr, "shutdown: only 'now' is supported as a TIME argument\n");
            return 1;
        }
    }

    syscall_status_t status = sys_shutdown(restart ? TRUE : FALSE);
    fprintf(stderr, "shutdown: sys_shutdown failed: %d\n", status);
    return 1;
}
