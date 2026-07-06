#pragma once

#include "job.h"

// Returns 0 if `list` was populated with one or more `&&`-chained runnable
// pipelines, 1 if the line was empty (nothing to run, not an error), -1 on a
// syntax error.
int parse_line(const char *raw_line, job_list_t *list);

// Frees every heap-owned string inside `job` (argv entries, redirect paths).
void job_free(job_t *job);

// Calls job_free() on every job in the list.
void job_list_free(job_list_t *list);

char *sh_strdup(const char *s);
