#include <stdio.h>
#include <string.h>
#include <signal.h>
#include <sys/syscall.h>

#include "parse.h"
#include "exec.h"
#include "builtins.h"

#define LINE_BUF_SIZE 256

static void sigint_handler(void *ctx) {
    (void)ctx;
}

static void run_job(sh_ctx_t *ctx, job_t *job) {
    // Builtins only handle the simple "single stage, no redirects" case —
    // they run directly in the shell and never go through exec_job's fd
    // wiring, so a redirected/piped builtin falls through to path search
    // instead of silently ignoring the redirect.
    if (job->stage_count == 1 && job->stages[0].redirect_count == 0) {
        int should_exit = 0;
        if (sh_run_builtin(ctx, job->stages[0].argv, job->stages[0].argc, &should_exit)) {
            return;
        }
    }
    exec_job(ctx, job);
}

int main(int argc, char *argv[]) {
    (void)argc; (void)argv;
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

    char line[LINE_BUF_SIZE];
    size_t line_len = 0;

    printf("$ ");
    for (;;) {
        char buf[64];
        size_t n = 0;
        syscall_status_t ret = sys_read(ctx.tty, buf, sizeof(buf), &n);

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

                job_t job;
                int rc = parse_line(line, &job);
                if (rc == 0) {
                    run_job(&ctx, &job);
                    job_free(&job);
                }
                else if (rc < 0) {
                    printf("sh: syntax error\n");
                }

                line_len = 0;
                sh_reap_background_jobs(&ctx);
                printf("$ ");
            }
            else if (line_len < LINE_BUF_SIZE - 1) {
                line[line_len++] = c;
            }
        }
    }

    return 0;
}
