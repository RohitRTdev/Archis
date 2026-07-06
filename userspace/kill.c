#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/syscall.h>

typedef struct {
    const char *name;
    int num;
} sig_entry_t;

// We only have these many signals for now 
static const sig_entry_t SIGNAL_TABLE[] = {
    { "INT",  SIGINT  },
    { "FPE",  SIGFPE  },
    { "SEGV", SIGSEGV },
    { "ILL",  SIGILL  },
    { "KILL", SIGKILL },
    { "TTIN", SIGTTIN }
};
#define SIGNAL_TABLE_LEN (sizeof(SIGNAL_TABLE) / sizeof(SIGNAL_TABLE[0]))

typedef enum {
    MODE_SIGNAL = 0,
    MODE_TERMINATE_PROCESS,
    MODE_TERMINATE_THREAD
} kill_mode_t;

static const char *signal_name(int num) {
    if (num < 0 || (size_t)num >= SIGNAL_TABLE_LEN) return NULL;
    return SIGNAL_TABLE[num].name;
}

static int resolve_signal(const char *spec, int *out) {
    char *end;
    long v = strtol(spec, &end, 10);
    if (*end == '\0' && end != spec) {
        if (v < 0 || (size_t)v >= SIGNAL_TABLE_LEN) return -1;
        *out = (int)v;
        return 0;
    }

    const char *name = spec;
    if (strncmp(name, "SIG", 3) == 0) {
        name += 3;
    }

    for (size_t i = 0; i < SIGNAL_TABLE_LEN; i++) {
        if (strcmp(SIGNAL_TABLE[i].name, name) == 0) {
            *out = SIGNAL_TABLE[i].num;
            return 0;
        }
    }
    return -1;
}

static void print_signal_table(void) {
    for (size_t i = 0; i < SIGNAL_TABLE_LEN; i++) {
        printf("%2zu) SIG%s\n", i, SIGNAL_TABLE[i].name);
    }
}

static void print_help(void) {
    printf("Usage: kill [-s signame | -n signum | -signame] pid...\n");
    printf("       kill -l [number]\n");
    printf("       kill -L\n");
    printf("       kill -T [-e exit_code] pid...   (non-POSIX: force-terminate a process)\n");
    printf("       kill -t [-e exit_code] tid...   (non-POSIX: force-terminate a thread)\n\n");
    printf("Send a signal to (or force-terminate) each pid/tid.\n");
    printf("Default signal is KILL (Archis has no TERM-equivalent signal).\n\n");
    printf("  -s signame, -n signum, -signame  select the signal to send\n");
    printf("  -l [number]                      list signal names, or name for number\n");
    printf("  -L                                list signal names and numbers\n");
    printf("  -T                                force-terminate whole process(es), bypassing signals\n");
    printf("  -t                                force-terminate a single thread, bypassing signals\n");
    printf("  -e exit_code                      exit code for -T/-t (default 0)\n");
    printf("  -h, --help                        display this help and exit\n");
}

static const char *status_reason(syscall_status_t status) {
    switch (status) {
        case E_NOT_FOUND: return "No such process";
        case E_NOPERM:    return "Operation not permitted";
        case E_INVALID:   return "Invalid argument";
        default:          return "Operation failed";
    }
}

int main(int argc, char *argv[]) {
    kill_mode_t mode = MODE_SIGNAL;
    int signal_num = SIGKILL;
    int have_signal_spec = 0;
    long long exit_code = 0;
    int have_exit_code = 0;
    int list_mode = 0, list_all_signals = 0;
    long list_arg = -1;

    int argi = 1;

    while (argi < argc) {
        const char *arg = argv[argi];
        if (arg[0] != '-' || arg[1] == '\0') {
            break;
        }
        if (strcmp(arg, "--") == 0) {
            argi++;
            break;
        }

        if (strcmp(arg, "-l") == 0) {
            list_mode = 1;
            argi++;
            if (argi < argc) {
                char *end;
                long v = strtol(argv[argi], &end, 10);
                if (*end == '\0' && end != argv[argi]) {
                    list_arg = v;
                    argi++;
                }
            }
            continue;
        }
        if (strcmp(arg, "-L") == 0) {
            list_all_signals = 1;
            argi++;
            continue;
        }
        if (strcmp(arg, "-h") == 0 || strcmp(arg, "--help") == 0) {
            print_help();
            return 0;
        }
        if (strcmp(arg, "-s") == 0) {
            argi++;
            if (argi >= argc) { fprintf(stderr, "kill: option '-s' requires an argument\n"); return 1; }
            if (resolve_signal(argv[argi], &signal_num) != 0) {
                fprintf(stderr, "kill: %s: invalid signal specification\n", argv[argi]);
                return 1;
            }
            have_signal_spec = 1;
            argi++;
            continue;
        }
        if (strcmp(arg, "-n") == 0) {
            argi++;
            if (argi >= argc) { fprintf(stderr, "kill: option '-n' requires an argument\n"); return 1; }
            char *end;
            long v = strtol(argv[argi], &end, 10);
            if (*end != '\0' || v < 0 || (size_t)v >= SIGNAL_TABLE_LEN) {
                fprintf(stderr, "kill: %s: invalid signal number\n", argv[argi]);
                return 1;
            }
            signal_num = (int)v;
            have_signal_spec = 1;
            argi++;
            continue;
        }
        if (strcmp(arg, "-T") == 0 || strcmp(arg, "--terminate") == 0) {
            if (mode == MODE_TERMINATE_THREAD) {
                fprintf(stderr, "kill: cannot combine -T and -t\n");
                return 1;
            }
            mode = MODE_TERMINATE_PROCESS;
            argi++;
            continue;
        }
        if (strcmp(arg, "-t") == 0 || strcmp(arg, "--thread") == 0) {
            if (mode == MODE_TERMINATE_PROCESS) {
                fprintf(stderr, "kill: cannot combine -T and -t\n");
                return 1;
            }
            mode = MODE_TERMINATE_THREAD;
            argi++;
            continue;
        }
        if (strcmp(arg, "-e") == 0 || strcmp(arg, "--exit-code") == 0) {
            argi++;
            if (argi >= argc) { fprintf(stderr, "kill: option '-e' requires an argument\n"); return 1; }
            char *end;
            long long v = strtoll(argv[argi], &end, 10);
            if (*end != '\0') {
                fprintf(stderr, "kill: %s: invalid exit code\n", argv[argi]);
                return 1;
            }
            exit_code = v;
            have_exit_code = 1;
            argi++;
            continue;
        }

        // Combined form: -9, -KILL, -SIGKILL, etc.
        if (resolve_signal(arg + 1, &signal_num) != 0) {
            fprintf(stderr, "kill: %s: invalid signal specification\n", arg);
            return 1;
        }
        have_signal_spec = 1;
        argi++;
    }

    if (list_all_signals) {
        print_signal_table();
        return 0;
    }
    if (list_mode) {
        if (list_arg >= 0) {
            const char *name = signal_name((int)list_arg);
            if (!name) {
                fprintf(stderr, "kill: %ld: invalid signal number\n", list_arg);
                return 1;
            }
            printf("%s\n", name);
        } else {
            print_signal_table();
        }
        return 0;
    }

    if (have_signal_spec && mode != MODE_SIGNAL) {
        fprintf(stderr, "kill: cannot combine a signal with -T/-t\n");
        return 1;
    }
    if (have_exit_code && mode == MODE_SIGNAL) {
        fprintf(stderr, "kill: -e is only valid with -T or -t\n");
        return 1;
    }

    if (argi >= argc) {
        print_help();
        return 1;
    }

    int fail = 0;
    for (; argi < argc; argi++) {
        char *end;
        long long target = strtoll(argv[argi], &end, 10);
        if (*end != '\0') {
            fprintf(stderr, "kill: %s: arguments must be process or thread IDs\n", argv[argi]);
            fail = 1;
            continue;
        }

        syscall_status_t status;
        if (mode == MODE_TERMINATE_PROCESS) {
            status = sys_terminate_process(target, exit_code);
        } else if (mode == MODE_TERMINATE_THREAD) {
            status = sys_terminate_thread(target, exit_code);
        } else {
            status = sys_issue_signal(target, (uint8_t)signal_num);
        }

        if (status != E_SUCCESS) {
            fprintf(stderr, "kill: (%s): %s\n", argv[argi], status_reason(status));
            fail = 1;
        }
    }

    return fail ? 1 : 0;
}
