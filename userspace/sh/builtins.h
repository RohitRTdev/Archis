#pragma once

#include "exec.h"

// Returns 1 if argv[0] named a builtin (and it ran), 0 if it's an external
// command the caller should exec instead. Sets *should_exit on `exit`.
int sh_run_builtin(sh_ctx_t *ctx, char *const argv[], int argc, int *should_exit);
