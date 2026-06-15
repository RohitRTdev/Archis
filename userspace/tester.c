#include <sys/syscall.h>

int main(int argc, const char* argv[]) {
    sys_print("tester: printing args");
    for (int i = 0; i < argc; i++) {
        sys_print(argv[i]);
    }

    return 0;
}