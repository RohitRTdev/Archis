#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>

static const char *status_str(uint8_t s) {
    switch (s) {
        case PROC_STATUS_READY: return "R";
        case PROC_STATUS_SUSPENDED: return "S";
        case PROC_STATUS_TERMINATED: return "Z";
        default: return "?";
    }
}

static void print_cmdline(uint64_t pid) {
    intf_process_request_t req = {0};
    req.type = INTF_PROCESS_COMMAND_LINE;
    req.pid = pid;

    if (sys_intf_request("process", &req) != E_SUCCESS || req.bytes_needed == 0) {
        return;
    }

    char *buf = malloc(req.bytes_needed);
    if (!buf) {
        return;
    }
    req.buffer = buf;
    if (sys_intf_request("process", &req) == E_SUCCESS && req.bytes_written > 0) {
        // bytes_written includes the NUL terminator written by the kernel handler.
        printf("%s", buf);
    }
    free(buf);
}

int main(void) {
    intf_process_request_t req = {0};
    req.type = INTF_PROCESS_GENERAL_INFO;

    if (sys_intf_request("process", &req) != E_SUCCESS) {
        fprintf(stderr, "ps: failed to query process list\n");
        return 1;
    }

    printf("PID   PPID  PGID  SID  THR STAT CMD\n");
    if (req.bytes_needed == 0) {
        return 0;
    }

    intf_process_info_t *procs = malloc(req.bytes_needed);
    if (!procs) {
        fprintf(stderr, "ps: out of memory\n");
        return 1;
    }

    req.buffer = procs;
    if (sys_intf_request("process", &req) != E_SUCCESS) {
        fprintf(stderr, "ps: failed to read process list\n");
        free(procs);
        return 1;
    }

    size_t count = req.bytes_written / sizeof(intf_process_info_t);
    for (size_t i = 0; i < count; i++) {
        printf("%-5llu %-5llu %-5llu %-5llu %-3llu %-4s ",
            (unsigned long long)procs[i].pid, (unsigned long long)procs[i].ppid,
            (unsigned long long)procs[i].pgid, (unsigned long long)procs[i].sid,
            (unsigned long long)procs[i].num_threads, status_str(procs[i].status));
        print_cmdline(procs[i].pid);
        printf("\n");
    }

    free(procs);
    return 0;
}
