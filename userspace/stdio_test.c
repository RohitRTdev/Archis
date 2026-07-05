#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>

int main(int argc, char *argv[]) {
    (void)argc;
    (void)argv;
    int fail = 0;

    FILE *f = fopen("/stdio_test.tmp", "w");
    if (!f) {
        printf("FAIL: fopen(w)\n");
        fail++;
    } else {
        const char *msg = "hello stdio\n";
        size_t n = fwrite(msg, 1, strlen(msg), f);
        if (n != strlen(msg)) {
            printf("FAIL: fwrite wrote %d\n", (int)n);
            fail++;
        }
        fclose(f);

        f = fopen("/stdio_test.tmp", "r");
        if (!f) {
            printf("FAIL: fopen(r)\n");
            fail++;
        } else {
            char buf[64];
            char *r = fgets(buf, sizeof(buf), f);
            if (!r || strcmp(buf, msg) != 0) {
                printf("FAIL: fgets got '%s'\n", r ? r : "(null)");
                fail++;
            } else {
                printf("PASS: fwrite/fread round trip\n");
            }
            fclose(f);
        }
        remove("/stdio_test.tmp");
    }

    int a = 0, b = 0;
    char name[32] = { 0 };
    int got = sscanf("42 hello 7", "%d %s %d", &a, name, &b);
    if (got == 3 && a == 42 && strcmp(name, "hello") == 0 && b == 7) {
        printf("PASS: sscanf\n");
    } else {
        printf("FAIL: sscanf got=%d a=%d name=%s b=%d\n", got, a, name, b);
        fail++;
    }

    char cwd[PATH_MAX];
    if (!getcwd(cwd, sizeof(cwd))) {
        printf("FAIL: getcwd\n");
        fail++;
    } else {
        printf("cwd before chdir: %s\n", cwd);
    }

    if (chdir("/bin") != 0) {
        printf("FAIL: chdir(/)\n");
        fail++;
    } else if (!getcwd(cwd, sizeof(cwd)) || strcmp(cwd, "/bin") != 0) {
        printf("FAIL: getcwd after chdir got '%s'\n", cwd);
        fail++;
    } else {
        printf("PASS: chdir/getcwd round trip\n");
    }

    printf("stdin fd=%lld stdout fd=%lld\n", (long long)stdin->fd, (long long)stdout->fd);
    printf(fail == 0 ? "ALL TESTS PASSED\n" : "SOME TESTS FAILED\n");

    return fail;
}
