#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>

#define ACCUM_SIZE 4096

int main(int argc, char *argv[]) {
    (void)argc; (void)argv;

    // Retry until the producer has created the named pipe
    handle_t rh;
    do {
        rh = sys_open("pipe", "myfifo", 0);
        if (rh < 0)
            sys_delay_ms(10);
    } while (rh < 0);

    char accum[ACCUM_SIZE];
    size_t acc_len = 0;

    while (acc_len < ACCUM_SIZE - 1) {
        char buf[128];
        size_t n = 0;
        syscall_status_t ret = sys_read(rh, buf, sizeof(buf) - 1, &n);
        if (ret < 0 || n == 0)
            break;

        buf[n] = '\0';
        printf("%s", buf);

        if (acc_len + n < ACCUM_SIZE) {
            memcpy(accum + acc_len, buf, n);
            acc_len += n;
            accum[acc_len] = '\0';
        }

        if (strstr(accum, "DONE\n"))
            break;
    }

    sys_close(rh);
    return 0;
}
