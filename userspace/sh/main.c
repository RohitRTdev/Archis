#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <pthread.h>
#include <sys/syscall.h>

#include "parse.h"
#include "exec.h"
#include "builtins.h"

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

// Runs one job (builtin or external), returning its exit status (0 = success)
// for use by run_job_list's `&&` short-circuiting.
static int run_job(sh_ctx_t *ctx, job_t *job, int grab_fg) {
    // Builtins only handle the simple "single stage, no redirects" case —
    // they run directly in the shell and never go through exec_job_fg's fd
    // wiring, so a redirected/piped builtin falls through to path search
    // instead of silently ignoring the redirect.
    if (job->stage_count == 1 && job->stages[0].redirect_count == 0) {
        int should_exit = 0, status = 0;
        if (sh_run_builtin(ctx, job->stages[0].argv, job->stages[0].argc, &should_exit, &status)) {
            return status;
        }
    }
    return exec_job_fg(ctx, job, grab_fg);
}

typedef struct {
    sh_ctx_t *ctx;
    job_list_t list;
} bg_chain_ctx_t;

// Entry point for a backgrounded `&&` chain (job_count > 1). Runs every
// segment to completion in this dedicated thread -- never grabbing the tty's
// foreground pgrp -- short-circuiting on the first nonzero status, then
// reports completion and frees its own heap-owned copy of the chain.
static void *run_bg_chain(void *arg) {
    bg_chain_ctx_t *bc = (bg_chain_ctx_t *)arg;

    for (int i = 0; i < bc->list.job_count; i++) {
        if (run_job(bc->ctx, &bc->list.jobs[i], 0) != 0) break;
    }

    printf("[bg]+ Done\n");
    job_list_free(&bc->list);
    free(bc);
    return NULL;
}

static void run_job_list(sh_ctx_t *ctx, job_list_t *list) {
    if (list->job_count == 1) {
        // Unchanged from before `&&` existed: a lone job either backgrounds
        // itself the traditional way (registered in ctx->bg_jobs) or runs
        // in the foreground; there's nothing to short-circuit against.
        if (list->background) {
            exec_job_bg(ctx, &list->jobs[0]);
        } else {
            run_job(ctx, &list->jobs[0], 1);
        }
        return;
    }

    if (!list->background) {
        for (int i = 0; i < list->job_count; i++) {
            if (run_job(ctx, &list->jobs[i], 1) != 0) break;
        }
        return;
    }

    // Backgrounded chain: hand ownership of the parsed jobs to a detached
    // thread so the interactive prompt is never blocked waiting on it.
    bg_chain_ctx_t *bc = malloc(sizeof(bg_chain_ctx_t));
    if (!bc) {
        printf("sh: out of memory, running chain in foreground\n");
        for (int i = 0; i < list->job_count; i++) {
            if (run_job(ctx, &list->jobs[i], 1) != 0) break;
        }
        return;
    }
    bc->ctx = ctx;
    bc->list = *list;
    list->job_count = 0; // ownership moved to bc; caller's job_list_free is now a no-op

    pthread_t tid;
    if (pthread_create(&tid, NULL, run_bg_chain, bc) != 0) {
        printf("sh: failed to background chain, running in foreground\n");
        for (int i = 0; i < bc->list.job_count; i++) {
            if (run_job(ctx, &bc->list.jobs[i], 1) != 0) break;
        }
        job_list_free(&bc->list);
        free(bc);
        return;
    }
    pthread_detach(tid);
    printf("[bg] chain started\n");
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

    printf("$ ");
    for (;;) {
        if (!read_line(&ctx, line, sizeof(line))) {
            // Ctrl-C fired while blocked waiting for input; read_line already
            // redrew the prompt on a new line, and the ring's in-progress
            // line was discarded tty-side (see tty_input's CTRL_C handling).
            continue;
        }

        job_list_t list;
        int rc = parse_line(line, &list);
        if (rc == 0) {
            run_job_list(&ctx, &list);
            job_list_free(&list);
        }
        else if (rc < 0) {
            printf("sh: syntax error\n");
        }

        sh_reap_background_jobs(&ctx);
        printf("$ ");
    }

    return 0;
}
