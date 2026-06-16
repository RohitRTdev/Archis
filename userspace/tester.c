#include <sys/syscall.h>
#include <stdio.h>

int main(int argc, const char* argv[]) {
    uint64_t pid = sys_get_pid();
    printf("tester with pid(%lu): printing args", pid);
    for (int i = 0; i < argc; i++) {
        printf("%s", argv[i]);
    }

    printf("Exiting tester");
    return 0;
}