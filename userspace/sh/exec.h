#pragma once

#include "job.h"

typedef struct {
    handle_t tty;
    size_t shell_pid;
    bg_job_t bg_jobs[SH_MAX_BG_JOBS];
} sh_ctx_t;

// Runs `job` to completion (foreground) or registers it in ctx->bg_jobs for
// later reaping (job->background). Caller still owns `job` and must
// job_free() it regardless of how this returns.
void exec_job(sh_ctx_t *ctx, job_t *job);

// Non-blocking poll of every tracked background job; prints a done/
// signal-death line and frees the slot for anything that has finished.
void sh_reap_background_jobs(sh_ctx_t *ctx);
