#include <string.h>
#include <stdio.h>
#include <stdlib.h>

#include "redir.h"

static int is_direct_path(const char *name) {
    return name[0] == '/' ||
           (name[0] == '.' && name[1] == '/') ||
           (name[0] == '.' && name[1] == '.' && name[2] == '/');
}

handle_t sh_create_process_path_search(char *const argv[], int argc, char *const envp[], uint64_t flags) {
    if (is_direct_path(argv[0])) {
        handle_t h = sys_create_process(argv, argc, envp, flags);
        if (h < 0) printf("sh: %s: not found\n", argv[0]);
        return h;
    }

    char *saved_argv0 = argv[0];
    char *argv_copy[SH_MAX_ARGS + 1];
    for (int i = 0; i <= argc; i++) argv_copy[i] = argv[i];

    const char *path = getenv("PATH");
    char candidate[PATH_MAX];

    if (path) {
        const char *p = path;
        while (*p) {
            const char *start = p;
            while (*p && *p != ':') p++;
            size_t dlen = (size_t)(p - start);

            if (dlen > 0 && dlen + 1 + strlen(saved_argv0) < sizeof(candidate)) {
                memcpy(candidate, start, dlen);
                candidate[dlen] = '/';
                strcpy(candidate + dlen + 1, saved_argv0);

                argv_copy[0] = candidate;
                handle_t h = sys_create_process(argv_copy, argc, envp, flags);
                if (h >= 0) return h;
            }

            if (*p == ':') p++;
        }
    }

    printf("sh: command not found: %s\n", saved_argv0);
    return E_NOT_FOUND;
}

handle_t sh_resolve_redirect(const redirect_t *redirect, handle_t current[3]) {
    if (redirect->kind == REDIR_DUP_FD) {
        int t = redirect->dup_target_fd;
        if (t < 0 || t > 2) return E_INVALID;
        return current[t];
    }

    handle_t h;
    if (redirect->kind == REDIR_APPEND) {
        h = sys_create_file(redirect->path, CREATE_FILE_EXIST_FLAG);
        if (h < 0) h = sys_create_file(redirect->path, 0);
        if (h >= 0) sys_seek(h, 0, SEEK_END);
    }
    else {
        h = sys_create_file(redirect->path, 0);
    }

    if (h < 0) printf("sh: %s: cannot open\n", redirect->path);
    return h;
}
