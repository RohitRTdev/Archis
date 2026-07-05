#include <stdlib.h>
#include <string.h>

char **environ;

static int environ_is_heap;
static size_t environ_capacity;

static size_t environ_count(void) {
    size_t n = 0;
    if (environ) {
        while (environ[n]) n++;
    }
    return n;
}

static int name_matches(const char *entry, const char *name, size_t name_len) {
    return strncmp(entry, name, name_len) == 0 && entry[name_len] == '=';
}

char *getenv(const char *name) {
    if (!environ || !name) return NULL;

    size_t name_len = strlen(name);
    for (size_t i = 0; environ[i]; i++) {
        if (name_matches(environ[i], name, name_len)) {
            return environ[i] + name_len + 1;
        }
    }
    return NULL;
}

/* The kernel-provided environ array is packed tightly against the argv/envp
 * strings on the stack, with no room to grow in place. So on the first
 * mutation we copy it into a malloc'd, growable array; from then on
 * environ is always heap-owned. We also don't bother freeing replaced
 * entries, since they may point into that original stack block or be
 * caller-owned (via putenv), neither of which we can safely free. */
static int environ_reserve(size_t min_capacity) {
    size_t count = environ_count();

    if (!environ_is_heap) {
        size_t new_capacity = min_capacity > count + 1 ? min_capacity : count + 1;
        char **new_environ = malloc(new_capacity * sizeof(char *));
        if (!new_environ) return -1;

        for (size_t i = 0; i < count; i++) {
            new_environ[i] = environ[i];
        }
        new_environ[count] = NULL;

        environ = new_environ;
        environ_is_heap = 1;
        environ_capacity = new_capacity;
        return 0;
    }

    if (min_capacity > environ_capacity) {
        char **new_environ = realloc(environ, min_capacity * sizeof(char *));
        if (!new_environ) return -1;

        environ = new_environ;
        environ_capacity = min_capacity;
    }

    return 0;
}

static int environ_set_entry(const char *name, char *entry, int overwrite) {
    size_t name_len = strlen(name);
    size_t count = environ_count();

    for (size_t i = 0; i < count; i++) {
        if (name_matches(environ[i], name, name_len)) {
            if (!overwrite) {
                free(entry);
                return 0;
            }
            if (environ_reserve(count + 1) != 0) {
                free(entry);
                return -1;
            }
            environ[i] = entry;
            return 0;
        }
    }

    if (environ_reserve(count + 2) != 0) {
        free(entry);
        return -1;
    }

    environ[count] = entry;
    environ[count + 1] = NULL;
    return 0;
}

int setenv(const char *name, const char *value, int overwrite) {
    if (!name || !*name || strchr(name, '=')) return -1;

    size_t name_len = strlen(name);
    size_t value_len = strlen(value);
    char *entry = malloc(name_len + 1 + value_len + 1);
    if (!entry) return -1;

    memcpy(entry, name, name_len);
    entry[name_len] = '=';
    memcpy(entry + name_len + 1, value, value_len + 1);

    return environ_set_entry(name, entry, overwrite);
}

int putenv(char *string) {
    if (!string) return -1;
    char *eq = strchr(string, '=');
    if (!eq) return -1;

    size_t name_len = (size_t)(eq - string);
    size_t count = environ_count();

    for (size_t i = 0; i < count; i++) {
        if (name_matches(environ[i], string, name_len)) {
            if (environ_reserve(count + 1) != 0) return -1;
            environ[i] = string;
            return 0;
        }
    }

    if (environ_reserve(count + 2) != 0) return -1;

    environ[count] = string;
    environ[count + 1] = NULL;
    return 0;
}

int unsetenv(const char *name) {
    if (!name || !*name || strchr(name, '=')) return -1;

    size_t name_len = strlen(name);
    size_t count = environ_count();

    for (size_t i = 0; i < count; i++) {
        if (name_matches(environ[i], name, name_len)) {
            if (environ_reserve(count + 1) != 0) return -1;
            for (size_t j = i; j < count; j++) {
                environ[j] = environ[j + 1];
            }
            return 0;
        }
    }

    return 0;
}
