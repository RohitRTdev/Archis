#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>

#define PATH "/fs_stress.dat"
#define CHUNK_MAX 256
#define PROGRESS_EVERY 10

static uint32_t rng_state;

static uint32_t xorshift32(void) {
    uint32_t x = rng_state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    rng_state = x;
    return x;
}

static size_t rand_range(size_t lo, size_t hi) {
    return lo + (xorshift32() % (hi - lo + 1));
}

int main(void) {
    size_t seed_ms = 0;
    sys_get_time_ms(CLOCK_MONOTONIC, &seed_ms);
    rng_state = (uint32_t)seed_ms | 1;

    uint8_t buf[CHUNK_MAX];

    handle_t h = sys_create_file(PATH, 0);
    if (h < 0) {
        fprintf(stderr, "fs_stress: initial sys_create_file failed: %lld\n", (long long)h);
        return 1;
    }

    unsigned long iters = 0;
    unsigned long aborted = 0;

    for (;;) {
        int action = xorshift32() % 4;

        switch (action) {
            case 0: case 1: {
                size_t len = rand_range(1, CHUNK_MAX);
                for (size_t i = 0; i < len; i++) buf[i] = (uint8_t)xorshift32();
                size_t written = 0;
                syscall_status_t s = sys_write(h, buf, len, &written);
                if (s != E_SUCCESS) aborted++;
                break;
            }
            case 2: {
                size_t len = rand_range(1, CHUNK_MAX);
                size_t got = 0;
                syscall_status_t s = sys_read(h, buf, len, &got);
                if (s != E_SUCCESS) aborted++;
                break;
            }
            case 3: {
                ssize_t off = (ssize_t)rand_range(0, 4096);
                sys_seek(h, off, SEEK_SET);
                break;
            }
        }


        if (rand_range(0, 63) == 0) {
            sys_close(h);
            h = sys_create_file(PATH, 0);
            if (h < 0) {
                fprintf(stderr, "fs_stress: recreate failed: %lld\n", (long long)h);
                sys_delay_ms(200);
                h = sys_create_file(PATH, 0);
                if (h < 0) continue;
            }
        }

        iters++;
        if (iters % PROGRESS_EVERY == 0) {
            printf("fs_stress: %lu iterations, %lu aborted\n", iters, aborted);
        }

        sys_delay_ms(20);
    }
}
