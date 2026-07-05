#include <stdio.h>
#include <stdlib.h>

extern int main(int argc, char *argv[], char *envp[]);

int crt_main(int argc, char *argv[], char *envp[]) {
    stdio_init_std_handles();
    environ = envp;
    return main(argc, argv, envp);
}
