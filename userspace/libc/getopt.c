#include <string.h>
#include <stdio.h>
#include <unistd.h>
#include <getopt.h>

char *optarg = 0;
int optind = 1;
int opterr = 1;
int optopt = 0;

// Points into the argv token currently being scanned for a clustered run of
// short options (e.g. "-abc"); NULL/empty means "advance to the next argv token".
static char *nextchar = 0;

static int handle_short_opt(int argc, char *const argv[], const char *optstring) {
    char c = *nextchar++;
    const char *spec = (c == ':') ? 0 : strchr(optstring, c);

    if (!spec) {
        optopt = c;
        if (*nextchar == '\0') nextchar = 0;
        if (opterr && optstring[0] != ':') {
            fprintf(stderr, "%s: invalid option -- '%c'\n", argv[0], c);
        }
        return '?';
    }

    if (spec[1] == ':') {
        if (*nextchar != '\0') {
            optarg = nextchar;
            nextchar = 0;
        } else if (optind < argc) {
            optarg = argv[optind++];
            nextchar = 0;
        } else {
            optopt = c;
            nextchar = 0;
            if (opterr && optstring[0] != ':') {
                fprintf(stderr, "%s: option requires an argument -- '%c'\n", argv[0], c);
            }
            return optstring[0] == ':' ? ':' : '?';
        }
    } else if (*nextchar == '\0') {
        nextchar = 0;
    }

    return (unsigned char)c;
}

static int handle_long_opt(int argc, char *const argv[], const char *optstring,
                            const struct option *longopts, int *longindex) {
    char *arg = argv[optind];
    char *name = arg + 2;
    char *eq = strchr(name, '=');
    size_t namelen = eq ? (size_t)(eq - name) : strlen(name);

    optind++;

    int found = -1;
    for (int i = 0; longopts[i].name; i++) {
        if (strlen(longopts[i].name) == namelen && strncmp(longopts[i].name, name, namelen) == 0) {
            found = i;
            break;
        }
    }

    if (found < 0) {
        optopt = 0;
        if (opterr) fprintf(stderr, "%s: unrecognized option '--%s'\n", argv[0], name);
        return '?';
    }

    const struct option *opt = &longopts[found];
    if (longindex) *longindex = found;

    if (opt->has_arg == required_argument) {
        if (eq) {
            optarg = eq + 1;
        } else if (optind < argc) {
            optarg = argv[optind++];
        } else {
            if (opterr && optstring[0] != ':') {
                fprintf(stderr, "%s: option '--%s' requires an argument\n", argv[0], opt->name);
            }
            return optstring[0] == ':' ? ':' : '?';
        }
    } else if (opt->has_arg == optional_argument) {
        optarg = eq ? eq + 1 : 0;
    } else {
        optarg = 0;
    }

    if (opt->flag) {
        *opt->flag = opt->val;
        return 0;
    }
    return opt->val;
}

static int getopt_internal(int argc, char *const argv[], const char *optstring,
                            const struct option *longopts, int *longindex) {
    optarg = 0;

    if (nextchar && *nextchar) {
        return handle_short_opt(argc, argv, optstring);
    }
    nextchar = 0;

    if (optind >= argc) return -1;

    char *arg = argv[optind];

    if (arg[0] != '-' || arg[1] == '\0') return -1;

    if (strcmp(arg, "--") == 0) {
        optind++;
        return -1;
    }

    if (longopts && arg[1] == '-') {
        return handle_long_opt(argc, argv, optstring, longopts, longindex);
    }

    nextchar = arg + 1;
    optind++;
    return handle_short_opt(argc, argv, optstring);
}

int getopt(int argc, char *const argv[], const char *optstring) {
    return getopt_internal(argc, argv, optstring, 0, 0);
}

int getopt_long(int argc, char *const argv[], const char *optstring,
                 const struct option *longopts, int *longindex) {
    return getopt_internal(argc, argv, optstring, longopts, longindex);
}
