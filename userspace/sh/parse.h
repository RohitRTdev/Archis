#pragma once

#include "job.h"

// Returns 0 if `job` was populated with a runnable pipeline, 1 if the line
// was empty (nothing to run, not an error), -1 on a syntax error.
int parse_line(const char *raw_line, job_t *job);

// Frees every heap-owned string inside `job` (argv entries, redirect paths).
void job_free(job_t *job);

char *sh_strdup(const char *s);
