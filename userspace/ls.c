#include <common.h>
#include <sys/syscall.h>
#include <stdio.h>

int main(void){
    printf("Hello from ls");
    sys_delay_ms(5000);
    int val = common_add(3,4);
    printf("Exiting ls");

    return val - 10;
}
