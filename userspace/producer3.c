#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/syscall.h>

static void write_all(handle_t wh, const char *msg) {
    size_t len = strlen(msg);
    size_t written = 0;
    while (written < len) {
        size_t n = 0;
        if (sys_write(wh, msg + written, len - written, &n) < 0)
            return;
        written += n;
    }
}

int main(int argc, char *argv[]) {
    if (argc < 2) {
        printf("[P3] Missing write handle argument\n");
        return -1;
    }

    handle_t wh = (handle_t)atoi(argv[1]);

    char buf[64];
    for (int i = 1; i <= 3; i++) {
        snprintf(buf, sizeof(buf), "[P3] Hello from producer 3 (inherited handle), iter %d\n", i);
        write_all(wh, buf);
    }

    return 0;
}
