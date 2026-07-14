#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <getopt.h>
#include <sys/syscall.h>

static void print_help(void) {
    printf("Usage: mount [-t TYPE] SOURCE TARGET\n");
    printf("       mount -u|--unmount TARGET\n");
    printf("Mount SOURCE (a device name, or \"tmpfs\") at TARGET.\n\n");
    printf("  -t, --types TYPE   filesystem type; \"tmpfs\" mounts a fresh\n");
    printf("                      in-memory filesystem, anything else names\n");
    printf("                      a device\n");
    printf("  -u, --unmount       unmount TARGET instead of mounting\n");
    printf("  -h, --help          display this help and exit\n");
}

static const char *error_message(syscall_status_t rc) {
    switch (rc) {
        case E_NOT_FOUND: return "no such device";
        case E_FILE_EXISTS: return "already mounted";
        case E_NOT_DIR: return "not a directory";
        case E_DEVICE_MOUNTED: return "device already mounted elsewhere";
        case E_FILE_BUSY: return "busy";
        case E_NOT_SUPPORTED: return "unrecognized filesystem";
        case E_INVALID: return "invalid argument";
        default: return "operation failed";
    }
}

int main(int argc, char *argv[]) {
    const char *type = NULL;
    int unmount = 0;

    static struct option long_opts[] = {
        {"help", no_argument, 0, 'h'},
        {"types", required_argument, 0, 't'},
        {"unmount", no_argument, 0, 'u'},
        {0, 0, 0, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "t:uh", long_opts, NULL)) != -1) {
        switch (opt) {
            case 't': type = optarg; break;
            case 'u': unmount = 1; break;
            case 'h': print_help(); return 0;
            default: return 1;
        }
    }

    if (unmount) {
        if (optind >= argc) {
            fprintf(stderr, "umount: missing operand\n");
            return 1;
        }
        const char *target = argv[optind];
        syscall_status_t rc = sys_unmount(target);
        if (rc != E_SUCCESS) {
            fprintf(stderr, "umount: %s: %s\n", target, error_message(rc));
            return 1;
        }
        return 0;
    }

    if (optind + 2 > argc) {
        fprintf(stderr, "mount: missing operand\n");
        return 1;
    }

    const char *source = argv[optind];
    const char *target = argv[optind + 1];
    if (type != NULL && strcmp(type, "tmpfs") == 0) {
        source = "tmpfs";
    }

    syscall_status_t rc = sys_mount(source, target);
    if (rc != E_SUCCESS) {
        fprintf(stderr, "mount: %s on %s: %s\n", source, target, error_message(rc));
        return 1;
    }

    return 0;
}
