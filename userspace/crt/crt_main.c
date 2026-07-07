#include <stdio.h>
#include <stdlib.h>
#include <signal.h>

extern int main(int argc, char *argv[], char *envp[]);

int crt_main(int argc, char *argv[], char *envp[]) {
    stdio_init_std_handles();
    signal_init();
    environ = envp;
    return main(argc, argv, envp);
}
