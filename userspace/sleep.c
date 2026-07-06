#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <getopt.h>
#include <sys/syscall.h>

#define OPT_HELP 1000

static void print_help(void) {
    printf("Usage: sleep NUMBER[SUFFIX]...\n");
    printf("Pause for NUMBER seconds, or the sum of the given arguments.\n\n");
    printf("SUFFIX may be 's' for seconds (default), 'm' for minutes, 'h' for\n");
    printf("hours, or 'd' for days.\n\n");
    printf("      --help    display this help and exit\n\n");
    printf("Only whole-second precision is supported (no fractional seconds).\n");
}

// Parses one NUMBER[SUFFIX] argument into milliseconds. Returns 0 and fills
// *out_ms on success, -1 on a malformed argument.
static int parse_duration_ms(const char *arg, unsigned long *out_ms) {
    char *end;
    long value = strtol(arg, &end, 10);
    if (end == arg || value < 0) return -1;

    unsigned long mult_s = 1;
    switch (*end) {
        case '\0': case 's': mult_s = 1;     break;
        case 'm':            mult_s = 60;    break;
        case 'h':            mult_s = 3600;  break;
        case 'd':            mult_s = 86400; break;
        default: return -1;
    }
    if (*end != '\0' && *(end + 1) != '\0') return -1;

    *out_ms = (unsigned long)value * mult_s * 1000;
    return 0;
}

int main(int argc, char *argv[]) {
    static struct option long_opts[] = {
        {"help", no_argument, 0, OPT_HELP},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "", long_opts, NULL)) != -1) {
        switch (opt) {
            case OPT_HELP: print_help(); return 0;
            default: return 1;
        }
    }

    if (optind >= argc) {
        fprintf(stderr, "sleep: missing operand\n");
        return 1;
    }

    unsigned long total_ms = 0;
    for (int i = optind; i < argc; i++) {
        unsigned long ms;
        if (parse_duration_ms(argv[i], &ms) != 0) {
            fprintf(stderr, "sleep: invalid time interval '%s'\n", argv[i]);
            return 1;
        }
        total_ms += ms;
    }

    syscall_status_t status = sys_delay_ms(total_ms);
    if (status == E_WAIT_INTERRUPTED) {
        return 1;
    }
    return 0;
}
