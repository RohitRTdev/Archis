#pragma once

#include "exec.h"

// Returns 1 if argv[0] named a builtin (and it ran), 0 if it's an external
// command the caller should exec instead. Sets *should_exit on `exit`, and
// *status to the builtin's exit status (0 = success) for use in `&&` chains.
int sh_run_builtin(sh_ctx_t *ctx, char *const argv[], int argc, int *should_exit, int *status);
