#include <string.h>
#include <ctype.h>
#include <stdlib.h>

#include "parse.h"

#define SH_LINE_BUF   256
#define SH_WORD_BUF   256
#define SH_MAX_TOKENS 64

typedef enum {
    TOK_WORD,
    TOK_PIPE,
    TOK_REDIR
} tok_type_t;

typedef struct {
    tok_type_t type;
    char *word;
    int fd;
    redir_kind_t kind;
    int dup_target_fd;
} token_t;

char *sh_strdup(const char *s) {
    size_t len = strlen(s);
    char *out = malloc(len + 1);
    if (!out) return NULL;
    memcpy(out, s, len + 1);
    return out;
}

static int is_word_end(char c) {
    return c == '\0' || isspace((unsigned char)c) || c == '|' || c == '>' || c == '&';
}

// Scans one $VAR/${VAR}-expanding word starting at *pp, writes the expanded
// text into a heap-allocated string and advances *pp past it.
static char *scan_word(const char **pp) {
    const char *p = *pp;
    char buf[SH_WORD_BUF];
    size_t blen = 0;

    while (!is_word_end(*p)) {
        if (*p == '$') {
            p++;
            char name[64];
            size_t nlen = 0;
            if (*p == '{') {
                p++;
                while (*p && *p != '}' && nlen < sizeof(name) - 1) name[nlen++] = *p++;
                if (*p == '}') p++;
            }
            else {
                while ((isalnum((unsigned char)*p) || *p == '_') && nlen < sizeof(name) - 1) name[nlen++] = *p++;
            }
            name[nlen] = '\0';

            const char *val = nlen ? getenv(name) : NULL;
            if (val) {
                size_t vlen = strlen(val);
                for (size_t i = 0; i < vlen && blen < SH_WORD_BUF - 1; i++) buf[blen++] = val[i];
            }
            continue;
        }
        if (blen < SH_WORD_BUF - 1) buf[blen++] = *p;
        p++;
    }
    buf[blen] = '\0';
    *pp = p;
    return sh_strdup(buf);
}

static void free_tokens(token_t *toks, int ntoks) {
    for (int i = 0; i < ntoks; i++) free(toks[i].word);
}

static int tokenize(const char *line, token_t *toks, int max_toks) {
    int ntoks = 0;
    const char *p = line;

    while (1) {
        while (isspace((unsigned char)*p)) p++;
        if (*p == '\0') break;
        if (ntoks >= max_toks) {
            free_tokens(toks, ntoks);
            return -1;
        }

        token_t *t = &toks[ntoks];
        t->word = NULL;

        if (*p == '|') {
            t->type = TOK_PIPE;
            p++;
            ntoks++;
            continue;
        }
        if (*p == '&') {
            free_tokens(toks, ntoks);
            return -1;
        }

        const char *save = p;
        int digits = 0;
        while (isdigit((unsigned char)*p)) { p++; digits++; }

        if (*p == '>') {
            int fd = digits ? atoi(save) : 1;
            p++;
            redir_kind_t kind = REDIR_TRUNC;
            int dup_target = -1;

            if (*p == '>') {
                kind = REDIR_APPEND;
                p++;
            }
            else if (*p == '&') {
                p++;
                const char *dstart = p;
                int ddig = 0;
                while (isdigit((unsigned char)*p)) { p++; ddig++; }
                if (ddig > 0) {
                    dup_target = atoi(dstart);
                    kind = REDIR_DUP_FD;
                }
                // else: no digits after '&' -- fall back to a plain file
                // redirect, identical to `>target` without the `&`
                // (kind stays REDIR_TRUNC, dup_target stays unused).
            }

            t->type = TOK_REDIR;
            t->fd = fd;
            t->kind = kind;
            t->dup_target_fd = dup_target;
            ntoks++;
            continue;
        }

        p = save;
        t->type = TOK_WORD;
        t->word = scan_word(&p);
        if (!t->word) {
            free_tokens(toks, ntoks);
            return -1;
        }
        ntoks++;
    }

    return ntoks;
}

static int build_job(token_t *toks, int ntoks, job_t *job) {
    stage_t *st = &job->stages[0];
    job->stage_count = 1;

    for (int i = 0; i < ntoks; i++) {
        token_t *t = &toks[i];

        if (t->type == TOK_PIPE) {
            st->argv[st->argc] = NULL;
            if (job->stage_count >= SH_MAX_STAGES) return -1;
            st = &job->stages[job->stage_count++];
            continue;
        }

        if (t->type == TOK_REDIR) {
            if (st->redirect_count >= SH_MAX_REDIRECTS) return -1;
            redirect_t *r = &st->redirects[st->redirect_count++];
            r->fd = t->fd;
            r->kind = t->kind;
            r->dup_target_fd = t->dup_target_fd;
            r->path = NULL;

            if (t->kind != REDIR_DUP_FD) {
                i++;
                if (i >= ntoks || toks[i].type != TOK_WORD) return -1;
                r->path = toks[i].word;
                toks[i].word = NULL;
            }
            continue;
        }

        if (st->argc >= SH_MAX_ARGS) return -1;
        st->argv[st->argc++] = t->word;
        t->word = NULL;
    }

    st->argv[st->argc] = NULL;
    return 0;
}

// Parses one `&&`-segment (no background/chain syntax of its own) into `job`.
// Returns 0 on success, -1 on a syntax error (including an empty segment --
// e.g. the gap in "a && && b" or a trailing "a &&").
static int parse_segment(const char *seg, job_t *job) {
    memset(job, 0, sizeof(*job));

    token_t toks[SH_MAX_TOKENS];
    int ntoks = tokenize(seg, toks, SH_MAX_TOKENS);
    if (ntoks < 0) return -1;
    if (ntoks == 0) return -1;

    int rc = build_job(toks, ntoks, job);
    if (rc != 0) {
        free_tokens(toks, ntoks);
        job_free(job);
        return -1;
    }

    return 0;
}

int parse_line(const char *raw_line, job_list_t *list) {
    memset(list, 0, sizeof(*list));

    char line[SH_LINE_BUF];
    strncpy(line, raw_line, sizeof(line) - 1);
    line[sizeof(line) - 1] = '\0';

    size_t len = strlen(line);
    while (len > 0 && isspace((unsigned char)line[len - 1])) len--;
    line[len] = '\0';

    if (len > 0 && line[len - 1] == '&') {
        list->background = 1;
        len--;
        line[len] = '\0';
        while (len > 0 && isspace((unsigned char)line[len - 1])) { len--; line[len] = '\0'; }
    }

    if (len == 0) return 1;

    // Split on top-level "&&" in place: the first '&' of each match becomes
    // the NUL terminator for the segment before it; the second '&' is
    // skipped and the next segment starts right after it. A lone '&' that
    // isn't part of "&&" is still rejected -- by parse_segment's tokenizer,
    // which errors on any stray '&' it encounters.
    const char *seg_start = line;
    char *p = line;
    while (1) {
        char *amp = strstr(p, "&&");
        if (!amp) break;

        if (list->job_count >= SH_MAX_CHAIN) { job_list_free(list); return -1; }
        *amp = '\0';
        if (parse_segment(seg_start, &list->jobs[list->job_count]) != 0) {
            job_list_free(list);
            return -1;
        }
        list->job_count++;

        p = amp + 2;
        seg_start = p;
    }

    if (list->job_count >= SH_MAX_CHAIN) { job_list_free(list); return -1; }
    if (parse_segment(seg_start, &list->jobs[list->job_count]) != 0) {
        job_list_free(list);
        return -1;
    }
    list->job_count++;

    return 0;
}

void job_free(job_t *job) {
    for (int i = 0; i < job->stage_count; i++) {
        stage_t *st = &job->stages[i];
        for (int a = 0; a < st->argc; a++) free(st->argv[a]);
        for (int r = 0; r < st->redirect_count; r++) free(st->redirects[r].path);
    }
}

void job_list_free(job_list_t *list) {
    for (int i = 0; i < list->job_count; i++) job_free(&list->jobs[i]);
}
