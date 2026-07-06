#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <signal.h>

#include "builtins.h"

static int builtin_cd(char *const argv[], int argc) {
    const char *target = argc > 1 ? argv[1] : "/";
    if (sys_chdir(target) < 0) {
        printf("sh: cd: %s: No such directory\n", target);
        return 1;
    }
    return 0;
}

static void builtin_pwd(void) {
    char buf[PATH_MAX];
    size_t n = 0;
    if (sys_getcwd(buf, sizeof(buf), &n) < 0) {
        printf("sh: pwd: failed\n");
        return;
    }
    printf("%s\n", buf);
}

static void builtin_export(char *const argv[], int argc) {
    for (int i = 1; i < argc; i++) {
        char *eq = strchr(argv[i], '=');
        if (!eq) continue;
        *eq = '\0';
        setenv(argv[i], eq + 1, 1);
        *eq = '=';
    }
}

// Kills every pid across every still-tracked background job individually
// (not via a single pgrp broadcast), so it's correct even if a pipeline's
// join-into-group step ever failed for one of its stages.
static void builtin_exit(sh_ctx_t *ctx) {
    for (int i = 0; i < SH_MAX_BG_JOBS; i++) {
        bg_job_t *bg = &ctx->bg_jobs[i];
        if (!bg->in_use) continue;
        for (int s = 0; s < bg->stage_count; s++) {
            sys_issue_signal((int64_t)bg->pids[s], SIGKILL);
        }
    }
    sys_exit(0);
}

int sh_run_builtin(sh_ctx_t *ctx, char *const argv[], int argc, int *should_exit, int *status) {
    *should_exit = 0;
    *status = 0;
    if (argc == 0) return 1;

    if (strcmp(argv[0], "cd") == 0) { *status = builtin_cd(argv, argc); return 1; }
    if (strcmp(argv[0], "pwd") == 0) { builtin_pwd(); return 1; }
    if (strcmp(argv[0], "export") == 0) { builtin_export(argv, argc); return 1; }
    if (strcmp(argv[0], "exit") == 0) { *should_exit = 1; builtin_exit(ctx); return 1; }

    return 0;
}
