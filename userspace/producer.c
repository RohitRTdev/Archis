#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <pthread.h>
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

static void *thread_fn(void *arg) {
    handle_t wh = (handle_t)(uintptr_t)arg;
    char buf[64];
    for (int i = 1; i <= 3; i++) {
        snprintf(buf, sizeof(buf), "[P1T] Hello from producer 1 thread, iter %d\n", i);
        write_all(wh, buf);
    }
    return NULL;
}

int main(int argc, char *argv[]) {
    (void)argc; (void)argv;

    handle_t read_h, write_h;
    if (sys_create_pipe(&read_h, &write_h, "myfifo", TRUE) < 0) {
        printf("[P1] Failed to create pipe\n");
        return -1;
    }
    // Producer 1 only writes; close the read end
    sys_close(read_h);

    // Spawn Producer 3
    char wh_str[16];
    snprintf(wh_str, sizeof(wh_str), "%d", (int)write_h);
    char *p3_args[] = {"/bin/producer3", wh_str};
    handle_t p3 = sys_create_process(p3_args, 2, NULL, 0);
    if (p3 < 0) {
        printf("[P1] Failed to spawn producer3\n");
        return -1;
    }

    // Spawn Producer 2 (opens the named pipe independently)
    char *p2_args[] = {"/bin/producer2"};
    handle_t p2 = sys_create_process(p2_args, 1, NULL, 0);
    if (p2 < 0) {
        printf("[P1] Failed to spawn producer2\n");
        return -1;
    }

    // Spin up a thread that also writes to the pipe
    pthread_t tid;
    pthread_create(&tid, NULL, thread_fn, (void *)(uintptr_t)write_h);

    // Write Producer 1's own messages
    char buf[64];
    for (int i = 1; i <= 3; i++) {
        snprintf(buf, sizeof(buf), "[P1] Hello from producer 1, iter %d\n", i);
        write_all(write_h, buf);
    }

    // Wait for all child producers and the thread
    sys_wait(p2, -1);
    sys_wait(p3, -1);
    pthread_join(tid, NULL);

    // Signal EOF to the consumer
    write_all(write_h, "DONE\n");
    sys_close(write_h);

    return 0;
}
