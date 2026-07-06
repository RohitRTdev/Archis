#pragma once

#include <sys/syscall.h>

#define SH_MAX_ARGS      16
#define SH_MAX_STAGES    8
#define SH_MAX_REDIRECTS 4
#define SH_MAX_BG_JOBS   16
#define SH_MAX_CHAIN     8

typedef enum {
    REDIR_TRUNC,  // >
    REDIR_APPEND, // >>
    REDIR_DUP_FD  // N>&M, e.g. 2>&1
} redir_kind_t;

typedef struct {
    int fd;
    redir_kind_t kind;
    char *path;        // heap-owned, only for REDIR_TRUNC/REDIR_APPEND
    int dup_target_fd;  // only for REDIR_DUP_FD
} redirect_t;

typedef struct {
    char *argv[SH_MAX_ARGS + 1];
    int argc;
    redirect_t redirects[SH_MAX_REDIRECTS];
    int redirect_count;
} stage_t;

typedef struct {
    stage_t stages[SH_MAX_STAGES];
    int stage_count;
} job_t;

typedef struct {
    job_t jobs[SH_MAX_CHAIN];
    int job_count;
    int background;   // trailing `&` -- applies to the entire chain
} job_list_t;

typedef struct {
    int in_use;
    size_t job_pgid;
    handle_t handles[SH_MAX_STAGES];
    size_t pids[SH_MAX_STAGES];
    char *names[SH_MAX_STAGES];
    int stage_count;
} bg_job_t;
