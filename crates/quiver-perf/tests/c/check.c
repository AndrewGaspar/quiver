/* Compile-only check of quiver_perf.h (C99 and C++, native and MinGW). The layout asserts in
 * the header fail the build if a struct doesn't match. */
#include "quiver_perf.h"

int main(void)
{
    qv_record r;
    qv_record_series(&r, 1, 0, 1.0);
    return (int)sizeof(qv_ring_header) - 4096 + (int)sizeof(qv_hud_buffer) - 256 + (r.nfields - 1);
}
