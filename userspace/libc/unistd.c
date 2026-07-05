#include <unistd.h>
#include <sys/syscall.h>

int chdir(const char *path) {
    return sys_chdir(path) == E_SUCCESS ? 0 : -1;
}

char *getcwd(char *buf, size_t size) {
    size_t written = 0;
    if (sys_getcwd(buf, size, &written) != E_SUCCESS) {
        return NULL;
    }
    return buf;
}
