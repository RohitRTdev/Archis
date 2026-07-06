#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>

int main(void) {
    intf_system_request_t req = {0};
    req.type = INTF_SYSTEM_KLOG;

    if (sys_intf_request("system", &req) != E_SUCCESS) {
        fprintf(stderr, "dmesg: failed to query log size\n");
        return 1;
    }
    if (req.bytes_needed == 0) {
        return 0;
    }

    char *buf = malloc(req.bytes_needed);
    if (!buf) {
        fprintf(stderr, "dmesg: out of memory\n");
        return 1;
    }

    req.buffer = buf;
    if (sys_intf_request("system", &req) != E_SUCCESS) {
        fprintf(stderr, "dmesg: failed to read log\n");
        free(buf);
        return 1;
    }

    fwrite(buf, 1, req.bytes_written, stdout);
    free(buf);
    return 0;
}
