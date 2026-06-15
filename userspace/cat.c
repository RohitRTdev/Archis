#include <common.h>
#include <sys/syscall.h>
int main(void) {
    sys_print("Hello from cat");

    char *const args[] = {"tester", "arg1", "arg2"};
    if (sys_create_process(args, 3) < 0) {
        sys_print("Process creation failed!");
    }

    sys_print("Exiting cat");
    return common_add(1,2);
}
