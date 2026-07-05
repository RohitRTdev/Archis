#include <stdio.h>

extern int main(int argc, char *argv[]);

int crt_main(int argc, char *argv[]) {
    stdio_init_std_handles();
    return main(argc, argv);
}
