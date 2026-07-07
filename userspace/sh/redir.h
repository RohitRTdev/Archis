#pragma once

#include "job.h"

// Tries argv[0] as-is if it starts with '/', "./" or "../"; otherwise walks
// $PATH, calling sys_create_process with each "dir/argv0" candidate (argv[0]
// itself is left untouched) until one succeeds. Prints "sh: command not
// found: <name>" and returns a negative status if every candidate fails.
handle_t sh_create_process_path_search(char *const argv[], int argc, char *const envp[], uint64_t flags);

// Resolves one redirect's target handle:
//  REDIR_TRUNC/REDIR_APPEND -> opens/creates redirect->path (append seeks to end)
//  REDIR_READ               -> opens redirect->path for reading (must already exist)
//  REDIR_DUP_FD             -> returns `current[redirect->dup_target_fd]`
// `current` holds the fd 0/1/2 handles resolved so far for this stage, so a
// DUP_FD redirect picks up whatever the referenced fd currently resolves to
// (left-to-right redirect order matters, same as a real shell).
handle_t sh_resolve_redirect(const redirect_t *redirect, handle_t current[3]);
