#include <common.h>
#include <sys/syscall.h>
int main(void){
    sys_print("Hello from ls", 13);
    sys_delay_ms(5000);
    int val = common_add(3,4);
    sys_print("Exiting ls", 10);

    return val - 10;
}
