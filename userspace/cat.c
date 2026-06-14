#include <common.h>
#include <sys/syscall.h>
int main(void) {
    sys_print("Hello from cat", 14);
    sys_print("Exiting cat", 11);
    return common_add(1,2);
}
