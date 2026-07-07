#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/syscall.h>

#include "exec.h"

#define LINE_BUF_SIZE 256

static void sigint_handler(void *ctx) {
    (void)ctx;
}

static int read_line(sh_ctx_t *ctx, char *line, size_t line_cap) {
    size_t line_len = 0;

    for (;;) {
        size_t n = 0;
        syscall_status_t ret = sys_read(ctx->tty, line + line_len, line_cap - 1 - line_len, &n);

        if (ret == E_WAIT_INTERRUPTED) {
            printf("\n$ ");
            return 0;
        }
        if (ret < 0 || n == 0) continue;

        line_len += n;
        if (line[line_len - 1] == '\n' || line_len >= line_cap - 1) break;
    }

    if (line_len > 0 && line[line_len - 1] == '\n') line_len--;
    line[line_len] = '\0';
    return 1;
}

int main(int argc, char *argv[]) {
    printf("sh: Starting shell process!\n");

    sh_ctx_t ctx;
    memset(&ctx, 0, sizeof(ctx));

    ctx.tty = sys_open("device", "tty", 0);
    if (ctx.tty < 0) {
        printf("sh: Unable to open tty device!\n");
        return -1;
    }

    ctx.shell_pid = (size_t)sys_get_pid();

    // Only the first/top-level sh (the one init spawns) should try to become a
    // session leader and take over the controlling tty. A nested sh launched
    // from an interactive prompt is already a process-group leader for its own
    // job by the time it gets here
    int is_session_leader = sys_create_sync_object(SYNC_EVENT, 0, 0, 0, 0, "sh.session_leader") >= 0;
    ctx.is_session_leader = is_session_leader;

    if (is_session_leader) {
        if (sys_set_session_leader(-1) < 0) {
            printf("sh: Unable to create new session!\n");
            return -1;
        }

        if (sys_device_control(ctx.tty, SET_CTTY, (void *)ctx.shell_pid) < 0) {
            printf("sh: Unable to set controlling tty for this session!\n");
            return -1;
        }

        if (sys_device_control(ctx.tty, SET_FOREGROUND_PGRP, (void *)ctx.shell_pid) < 0) {
            printf("sh: Unable to set sh as foreground process!\n");
            return -1;
        }
    }

    set_signal_handler(SIGINT, sigint_handler, 0);

    if (argc > 1) {
        // Script mode: run the file's lines to completion, then exit with
        // the last line's status. No prompt, no /conf/init_sh.sh.
        int status = 0;
        if (sh_run_script(&ctx, argv[1], &status) < 0) {
            printf("sh: %s: No such file or directory\n", argv[1]);
            return 1;
        }
        return status;
    }

    // Interactive mode: run the startup script before the first prompt (a
    // missing file isn't fatal -- the shell still starts, just without PATH
    // etc. preset).
    int init_status;
    sh_run_script(&ctx, "/conf/init_sh", &init_status);

    char line[LINE_BUF_SIZE];

    printf("$ ");
    for (;;) {
        if (!read_line(&ctx, line, sizeof(line))) {
            // Ctrl-C fired while blocked waiting for input; read_line already
            // redrew the prompt on a new line, and the ring's in-progress
            // line was discarded tty-side (see tty_input's CTRL_C handling).
            continue;
        }

        sh_run_line(&ctx, line);
        printf("$ ");
    }

    return 0;
}
