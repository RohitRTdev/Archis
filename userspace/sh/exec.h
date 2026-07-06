#pragma once

#include "job.h"

typedef struct {
    handle_t tty;
    size_t shell_pid;
    int is_session_leader;
    bg_job_t bg_jobs[SH_MAX_BG_JOBS];
} sh_ctx_t;

// Runs `job` to completion, waiting for every stage. If `grab_fg` is set,
// takes the tty's foreground process group before resuming the job and
// restores it to the shell's own pgid afterward -- pass 0 when running as
// part of a backgrounded `&&` chain, where the job must never own the tty.
// Caller still owns `job` and must job_free() it regardless of how this
// returns. Returns 0 if the job's last stage exited normally with code 0,
// nonzero otherwise (nonzero exit, signal death, or the job never launched).
int exec_job_fg(sh_ctx_t *ctx, job_t *job, int grab_fg);

// Launches `job` in the background: spawns it and registers it in
// ctx->bg_jobs for later polling/reporting via sh_reap_background_jobs,
// without waiting. Caller still owns `job` and must job_free() it regardless.
void exec_job_bg(sh_ctx_t *ctx, job_t *job);

// Non-blocking poll of every tracked background job; prints a done/
// signal-death line and frees the slot for anything that has finished.
void sh_reap_background_jobs(sh_ctx_t *ctx);
