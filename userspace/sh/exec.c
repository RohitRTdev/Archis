#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>

#include "exec.h"
#include "redir.h"
#include "parse.h"

extern char **environ;

static const char *signal_name(uint8_t sig) {
    switch (sig) {
        case SIGINT:  return "Interrupt";
        case SIGFPE:  return "Floating point exception";
        case SIGSEGV: return "Segmentation fault";
        case SIGILL:  return "Illegal instruction";
        case SIGKILL: return "Killed";
        case SIGTTIN: return "Stopped (tty input)";
        default:      return "Unknown signal";
    }
}

static void report_stage_status(const char *name, handle_t handle) {
    process_info_t info;
    if (sys_get_process_info(handle, &info) < 0) return;
    if (info.exit_info.reason == EXIT_SIGNAL) {
        printf("sh: %s: %s\n", name, signal_name((uint8_t)info.exit_info.code));
    }
}

void exec_job(sh_ctx_t *ctx, job_t *job) {
    int total = job->stage_count;
    handle_t pipe_r[SH_MAX_STAGES - 1];
    handle_t pipe_w[SH_MAX_STAGES - 1];
    handle_t stage_handles[SH_MAX_STAGES];
    size_t stage_pids[SH_MAX_STAGES];
    char *stage_names[SH_MAX_STAGES];
    handle_t opened_files[SH_MAX_STAGES * SH_MAX_REDIRECTS];
    int n_opened = 0;
    size_t job_pgid = 0;
    int created = 0;
    int aborted = 0;

    for (int i = 0; i < total - 1; i++) {
        if (sys_create_pipe(&pipe_r[i], &pipe_w[i], NULL, TRUE) < 0) {
            printf("sh: failed to create pipe\n");
            for (int j = 0; j < i; j++) { sys_close(pipe_r[j]); sys_close(pipe_w[j]); }
            return;
        }
    }

    for (created = 0; created < total; created++) {
        stage_t *st = &job->stages[created];
        handle_t h = sh_create_process_path_search(st->argv, st->argc, environ, PROCESS_SUSPEND_FLAG);
        if (h < 0) { aborted = 1; break; }

        stage_handles[created] = h;
        stage_names[created] = st->argv[0];

        process_info_t info;
        sys_get_process_info(h, &info);
        stage_pids[created] = info.id;

        if (created == 0) {
            sys_set_pgrp(h, 0);
            job_pgid = stage_pids[0];
        }
        else {
            // Remaining spawned processes in this job must 
            // be attached to the newly created process group
            sys_set_pgrp(h, job_pgid);
        }

        handle_t cur[3];
        cur[0] = (created == 0) ? ctx->tty : pipe_r[created - 1];
        cur[1] = (created == total - 1) ? ctx->tty : pipe_w[created];
        cur[2] = ctx->tty;

        int redirect_failed = 0;
        for (int r = 0; r < st->redirect_count; r++) {
            redirect_t *rd = &st->redirects[r];
            handle_t resolved = sh_resolve_redirect(rd, cur);
            if (resolved < 0) { redirect_failed = 1; continue; }
            cur[rd->fd] = resolved;
            if (rd->kind != REDIR_DUP_FD) opened_files[n_opened++] = resolved;
        }

        sys_duplicate_handle(h, cur[0], STDIN_FILENO, TRUE);
        sys_duplicate_handle(h, cur[1], STDOUT_FILENO, TRUE);
        sys_duplicate_handle(h, cur[2], STDERR_FILENO, TRUE);

        if (redirect_failed) { created++; aborted = 1; break; }
    }

    for (int i = 0; i < total - 1; i++) {
        sys_close(pipe_r[i]);
        sys_close(pipe_w[i]);
    }
    for (int i = 0; i < n_opened; i++) sys_close(opened_files[i]);

    if (aborted) {
        for (int i = 0; i < created; i++) {
            sys_issue_signal((int64_t)stage_pids[i], SIGKILL);
            sys_close(stage_handles[i]);
        }
        return;
    }

    if (!job->background) {
        sys_device_control(ctx->tty, SET_FOREGROUND_PGRP, (void *)job_pgid);
    }

    for (int i = 0; i < total; i++) sys_resume_process(stage_handles[i]);

    if (job->background) {
        int slot = -1;
        for (int i = 0; i < SH_MAX_BG_JOBS; i++) {
            if (!ctx->bg_jobs[i].in_use) { slot = i; break; }
        }
        if (slot < 0) {
            printf("sh: too many background jobs\n");
            for (int i = 0; i < total; i++) {
                sys_issue_signal((int64_t)stage_pids[i], SIGKILL);
                sys_close(stage_handles[i]);
            }
            return;
        }

        bg_job_t *bg = &ctx->bg_jobs[slot];
        bg->in_use = 1;
        bg->job_pgid = job_pgid;
        bg->stage_count = total;
        for (int i = 0; i < total; i++) {
            bg->handles[i] = stage_handles[i];
            bg->pids[i] = stage_pids[i];
            bg->names[i] = sh_strdup(stage_names[i]);
        }
        printf("[%d] %zu\n", slot + 1, job_pgid);
        return;
    }

    for (int i = 0; i < total; i++) sys_wait(stage_handles[i], -1);

    // Restore foreground process group back to our own process group
    sys_device_control(ctx->tty, SET_FOREGROUND_PGRP, (void *)ctx->shell_pid);
    sys_device_control(ctx->tty, SET_TTY_MODE, (void *)(size_t)(TTY_MODE_ECHO | TTY_MODE_CANON));

    for (int i = 0; i < total; i++) {
        report_stage_status(stage_names[i], stage_handles[i]);
        sys_close(stage_handles[i]);
    }
}

void sh_reap_background_jobs(sh_ctx_t *ctx) {
    for (int i = 0; i < SH_MAX_BG_JOBS; i++) {
        bg_job_t *bg = &ctx->bg_jobs[i];
        if (!bg->in_use) continue;

        int all_done = 1;
        for (int s = 0; s < bg->stage_count; s++) {
            if (sys_wait(bg->handles[s], 0) != E_SUCCESS) { all_done = 0; break; }
        }
        if (!all_done) continue;

        printf("[%d]+ Done %zu\n", i + 1, bg->job_pgid);
        for (int s = 0; s < bg->stage_count; s++) {
            report_stage_status(bg->names[s], bg->handles[s]);
            sys_close(bg->handles[s]);
            free(bg->names[s]);
        }
        bg->in_use = 0;
    }
}
