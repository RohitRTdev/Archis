#include <stdio.h>
#include <string.h>
#include <ctype.h>
#include <signal.h>
#include <sys/syscall.h>

#define LINE_BUF_SIZE 256
#define MAX_ARGS 16

static void sigint_handler(void *ctx) {
    (void)ctx;
}

static size_t split_args(char *line, char *out_argv[], size_t max_args) {
    size_t argc = 0;
    char *p = line;
    while (*p && argc < max_args) {
        while (*p && isspace((unsigned char)*p)) p++;
        if (!*p) break;
        out_argv[argc++] = p;
        while (*p && !isspace((unsigned char)*p)) p++;
        if (*p) {
            *p = '\0';
            p++;
        }
    }
    return argc;
}

int main(int argc, char* argv[]) {
    printf("Starting shell process!\n");

    handle_t tty_dev = sys_open("device", "tty", 0);
    if (tty_dev < 0) {
        printf("sh: Unable to open tty device!\n");
        return -1;
    }

    int pid = sys_get_pid();

    if (sys_set_session_leader(-1) < 0) {
        printf("sh: Unable to create new session!\n");
        return -1;
    }

    if (sys_device_control(tty_dev, SET_CTTY, (void*)pid) < 0) {
        printf("sh: Unable to set controlling tty for this session!\n");
        return -1;
    }

    if (sys_device_control(tty_dev, SET_FOREGROUND_PGRP, (void*)pid) < 0) {
        printf("sh: Unable to set sh as foreground process!\n");
        return -1;
    }

    set_signal_handler(SIGINT, sigint_handler, 0);

    char line[LINE_BUF_SIZE];
    size_t line_len = 0;

    printf("$ ");
    for (;;) {
        char buf[64];
        size_t n = 0;
        syscall_status_t ret = sys_read(tty_dev, buf, sizeof(buf), &n);

        if (ret == E_WAIT_INTERRUPTED) {
            // Ctrl-C fired while we were blocked waiting for input: drop
            // whatever was typed so far and redraw the prompt on a new line.
            line_len = 0;
            printf("\n$ ");
            continue;
        }
        if (ret < 0 || n == 0) {
            continue;
        }

        for (size_t i = 0; i < n; i++) {
            char c = buf[i];
            if (c == '\n') {
                line[line_len] = '\0';

                char *cmd_argv[MAX_ARGS];
                size_t cmd_argc = split_args(line, cmd_argv, MAX_ARGS);

                if (cmd_argc > 0) {
                    handle_t child = sys_create_process(cmd_argv, cmd_argc, NULL, 0);
                    if (child < 0) {
                        printf("sh: failed to launch %s\n", cmd_argv[0]);
                    }
                    else {
                        sys_wait(child, -1);
                    }
                }

                line_len = 0;
                printf("$ ");
            }
            else if (line_len < LINE_BUF_SIZE - 1) {
                line[line_len++] = c;
            }
        }
    }

    return 0;
}
