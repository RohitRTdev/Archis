#include <stdio.h>
#include <string.h>
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
    (void)argc; (void)argv;

    handle_t wh = sys_open("pipe", "myfifo", OPEN_WRITE_FLAG);
    if (wh < 0) {
        printf("[P2] Failed to open pipe for writing\n");
        return -1;
    }

    char buf[64];
    for (int i = 1; i <= 3; i++) {
        snprintf(buf, sizeof(buf), "[P2] Hello from producer 2, iter %d\n", i);
        write_all(wh, buf);
    }

    sys_close(wh);
    return 0;
}
