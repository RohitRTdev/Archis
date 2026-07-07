#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <pthread.h>

#include "exec.h"
#include "redir.h"
#include "parse.h"
#include "builtins.h"

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

// Spawns every stage of `job`, wiring pipes/redirects and setting each
// stage's process group (stage 0 becomes the new job's pgid). Every stage is
// created PROCESS_SUSPEND_FLAG'd -- the caller decides whether/when to grab
// the tty's foreground pgrp before resuming them.
//
// On success (*aborted == 0), fills stage_handles/stage_pids/stage_names (up
// to job->stage_count entries) and *job_pgid, and every stage is still
// suspended (not yet resumed). On failure (*aborted == 1), every partially
// spawned stage has already been killed and closed -- there is nothing left
// for the caller to clean up.
static void spawn_stages(
    sh_ctx_t *ctx, job_t *job,
    handle_t stage_handles[SH_MAX_STAGES],
    size_t stage_pids[SH_MAX_STAGES],
    char *stage_names[SH_MAX_STAGES],
    size_t *job_pgid,
    int *aborted
) {
    int total = job->stage_count;
    handle_t pipe_r[SH_MAX_STAGES - 1];
    handle_t pipe_w[SH_MAX_STAGES - 1];
    handle_t opened_files[SH_MAX_STAGES * SH_MAX_REDIRECTS];
    int n_opened = 0;
    int created = 0;
    *aborted = 0;
    *job_pgid = 0;

    for (int i = 0; i < total - 1; i++) {
        // Must stay non-inheritable
        // The writer process must not get the read handle 
        // and reader process must not get the write handle
        if (sys_create_pipe(&pipe_r[i], &pipe_w[i], NULL, FALSE) < 0) {
            printf("sh: failed to create pipe\n");
            for (int j = 0; j < i; j++) { sys_close(pipe_r[j]); sys_close(pipe_w[j]); }
            *aborted = 1;
            return;
        }
    }

    for (created = 0; created < total; created++) {
        stage_t *st = &job->stages[created];
        handle_t h = sh_create_process_path_search(st->argv, st->argc, environ, PROCESS_SUSPEND_FLAG);
        if (h < 0) { *aborted = 1; break; }

        stage_handles[created] = h;
        stage_names[created] = st->argv[0];

        process_info_t info;
        sys_get_process_info(h, &info);
        stage_pids[created] = info.id;

        if (created == 0) {
            sys_set_pgrp(h, 0);
            *job_pgid = stage_pids[0];
        }
        else {
            // Remaining spawned processes in this job must
            // be attached to the newly created process group
            sys_set_pgrp(h, *job_pgid);
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

        if (redirect_failed) { created++; *aborted = 1; break; }
    }

    for (int i = 0; i < total - 1; i++) {
        sys_close(pipe_r[i]);
        sys_close(pipe_w[i]);
    }
    for (int i = 0; i < n_opened; i++) sys_close(opened_files[i]);

    if (*aborted) {
        for (int i = 0; i < created; i++) {
            sys_issue_signal((int64_t)stage_pids[i], SIGKILL);
            sys_close(stage_handles[i]);
        }
    }
}

int exec_job_fg(sh_ctx_t *ctx, job_t *job, int grab_fg) {
    int total = job->stage_count;
    handle_t stage_handles[SH_MAX_STAGES];
    size_t stage_pids[SH_MAX_STAGES];
    char *stage_names[SH_MAX_STAGES];
    size_t job_pgid;
    int aborted;

    spawn_stages(ctx, job, stage_handles, stage_pids, stage_names, &job_pgid, &aborted);
    if (aborted) return -1;

    if (grab_fg) {
        sys_device_control(ctx->tty, SET_FOREGROUND_PGRP, (void *)job_pgid);
    }

    for (int i = 0; i < total; i++) sys_resume_process(stage_handles[i]);
    for (int i = 0; i < total; i++) sys_wait(stage_handles[i], -1);

    if (grab_fg) {
        // Restore foreground process group back to our own process group
        sys_device_control(ctx->tty, SET_FOREGROUND_PGRP, (void *)ctx->shell_pid);
        sys_device_control(ctx->tty, SET_TTY_MODE, (void *)(size_t)(TTY_MODE_ECHO | TTY_MODE_CANON));
    }

    int status = 0;
    for (int i = 0; i < total; i++) {
        process_info_t info;
        int have_info = sys_get_process_info(stage_handles[i], &info) >= 0;
        report_stage_status(stage_names[i], stage_handles[i]);
        if (i == total - 1 && have_info) {
            status = (info.exit_info.reason == EXIT_NORMAL && info.exit_info.code == 0) ? 0 : 1;
        }
        sys_close(stage_handles[i]);
    }

    return status;
}

void exec_job_bg(sh_ctx_t *ctx, job_t *job) {
    int total = job->stage_count;
    handle_t stage_handles[SH_MAX_STAGES];
    size_t stage_pids[SH_MAX_STAGES];
    char *stage_names[SH_MAX_STAGES];
    size_t job_pgid;
    int aborted;

    spawn_stages(ctx, job, stage_handles, stage_pids, stage_names, &job_pgid, &aborted);
    if (aborted) return;

    for (int i = 0; i < total; i++) sys_resume_process(stage_handles[i]);

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

// Dispatches a parsed job list, returning the exit status of the last
// foreground job run (0 for any backgrounded-launch path, matching a real
// shell reporting success immediately on `&` without waiting for it).
static int run_job_list(sh_ctx_t *ctx, job_list_t *list) {
    if (list->job_count == 1) {
        // Unchanged from before `&&` existed: a lone job either backgrounds
        // itself the traditional way (registered in ctx->bg_jobs) or runs
        // in the foreground; there's nothing to short-circuit against.
        if (list->background) {
            exec_job_bg(ctx, &list->jobs[0]);
            return 0;
        }
        return run_job(ctx, &list->jobs[0], 1);
    }

    if (!list->background) {
        int status = 0;
        for (int i = 0; i < list->job_count; i++) {
            status = run_job(ctx, &list->jobs[i], 1);
            if (status != 0) break;
        }
        return status;
    }

    // Backgrounded chain: hand ownership of the parsed jobs to a detached
    // thread so the interactive prompt is never blocked waiting on it.
    bg_chain_ctx_t *bc = malloc(sizeof(bg_chain_ctx_t));
    if (!bc) {
        printf("sh: out of memory, running chain in foreground\n");
        int status = 0;
        for (int i = 0; i < list->job_count; i++) {
            status = run_job(ctx, &list->jobs[i], 1);
            if (status != 0) break;
        }
        return status;
    }
    bc->ctx = ctx;
    bc->list = *list;
    list->job_count = 0; // ownership moved to bc; caller's job_list_free is now a no-op

    pthread_t tid;
    if (pthread_create(&tid, NULL, run_bg_chain, bc) != 0) {
        printf("sh: failed to background chain, running in foreground\n");
        int status = 0;
        for (int i = 0; i < bc->list.job_count; i++) {
            status = run_job(ctx, &bc->list.jobs[i], 1);
            if (status != 0) break;
        }
        job_list_free(&bc->list);
        free(bc);
        return status;
    }
    pthread_detach(tid);
    printf("[bg] chain started\n");
    return 0;
}

int sh_run_line(sh_ctx_t *ctx, char *line) {
    job_list_t list;
    int status = 0;
    int rc = parse_line(line, &list);
    if (rc == 0) {
        status = run_job_list(ctx, &list);
        job_list_free(&list);
    } else if (rc < 0) {
        printf("sh: syntax error\n");
        status = 1;
    }

    sh_reap_background_jobs(ctx);
    return status;
}

// Reads one line (without the newline) into a malloc'd, NUL-terminated
// buffer. Returns 1 with *out_buf/*out_len set, 0 on clean EOF (nothing
// read), -1 on allocation failure. Mirrors the FILE*-based line reader
// already duplicated in cat.c/head.c.
static int read_script_line(FILE *f, char **out_buf, size_t *out_len) {
    size_t cap = 128, len = 0;
    char *buf = malloc(cap);
    if (!buf) return -1;

    int c;
    int any = 0;
    while ((c = fgetc(f)) != EOF) {
        any = 1;
        if (c == '\n') break;
        if (len + 1 >= cap) {
            cap *= 2;
            char *nb = realloc(buf, cap);
            if (!nb) { free(buf); return -1; }
            buf = nb;
        }
        buf[len++] = (char)c;
    }

    if (!any) { free(buf); return 0; }

    buf[len] = '\0';
    *out_buf = buf;
    *out_len = len;
    return 1;
}

int sh_run_script(sh_ctx_t *ctx, const char *path, int *out_status) {
    FILE *f = fopen(path, "r");
    if (!f) return -1;

    int status = 0;
    for (;;) {
        char *line;
        size_t len;
        int rc = read_script_line(f, &line, &len);
        if (rc <= 0) break;
        (void)len;
        status = sh_run_line(ctx, line);
        free(line);
    }

    fclose(f);
    *out_status = status;
    return 0;
}
