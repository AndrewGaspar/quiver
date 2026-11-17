/* Writes records into an existing ring with quiver_perf.h's writer; tests/header.rs reads
 * them back with the Rust reader. Usage: ring_writer <ring file> <count> */
#define _POSIX_C_SOURCE 200809L
#include "quiver_perf.h"

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    struct stat st;
    qv_ring_header *h;
    uint32_t slots;
    long long i, n;
    int fd;
    void *p;

    if (argc != 3) return 64;
    n = atoll(argv[2]);
    fd = open(argv[1], O_RDWR);
    if (fd < 0 || fstat(fd, &st) != 0) return 65;
    p = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) return 66;
    h = (qv_ring_header *)p;
    if (!qv_ring_valid(h, (size_t)st.st_size)) return 67;
    slots = h->slot_count;
    for (i = 0; i < n; i++) {
        qv_record r;
        int64_t dur = i;
        int k;
        switch (i % 3) {
        case 0:
            memset(&r, 0, sizeof r);
            r.metric = 1;
            r.key = (uint64_t)i;
            r.flags = QV_FLAG_REAL;
            r.swapchain = 7;
            r.t = i * 1000;
            r.nfields = 3;
            for (k = 0; k < 3; k++) r.f[k] = i * 10 + k;
            break;
        case 1:
            qv_record_series(&r, 2, i * 1000, (double)i * 0.5);
            break;
        default:
            qv_record_event(&r, 3, i * 1000, &dur, "marker \xc3\xa9 with a label longer than forty-eight bytes");
            break;
        }
        if (!qv_ring_push(h, slots, &r)) return 68;
    }
    qv_ring_heartbeat(h, 12345);
    qv_ring_add_dropped(h, 2);
    printf("%u %u\n", (unsigned)sizeof(qv_ring_header), (unsigned)sizeof(qv_record));
    return 0;
}
