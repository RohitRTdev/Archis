#include <common.h>
#include <sys/syscall.h>
int main(void){
    sys_print("Hello from ls");
    sys_delay_ms(5000);
    int val = common_add(3,4);
    sys_print("Exiting ls");

    return val - 10;
}
