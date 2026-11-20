#include <stdint.h>
#include <sys/time.h>

static int64_t g_dyld_init_epoch_us = 0;

__attribute__((constructor))
static void aipinyin_record_dyld_init(void) {
    struct timeval tv;
    if (gettimeofday(&tv, 0) == 0) {
        g_dyld_init_epoch_us = (int64_t)tv.tv_sec * 1000000LL + (int64_t)tv.tv_usec;
    }
}

int64_t aipinyin_dyld_init_epoch_us(void) {
    return g_dyld_init_epoch_us;
}
