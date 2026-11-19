/* The virtual clock's arithmetic (vclock.h). Portable C11 with GCC atomics: built into the
 * hook DLL for x86_64 and i686 Windows, and for the host by the crate's tests. */
#include "vclock.h"

#include <string.h>

#define LOAD(p) __atomic_load_n((p), __ATOMIC_ACQUIRE)
#define STORE(p, v) __atomic_store_n((p), (v), __ATOMIC_RELEASE)
#define ADD(p, v) __atomic_add_fetch((p), (v), __ATOMIC_ACQ_REL)
#define XCHG(p, v) __atomic_exchange_n((p), (v), __ATOMIC_ACQ_REL)

/* Creep step: ticks_per_frame / CREEP_DIV per query past creep_after. */
#define CREEP_DIV 1024

uint64_t vc_load_u64(const uint64_t *p) { return LOAD(p); }
int64_t vc_load_i64(const int64_t *p) { return LOAD(p); }

/* x * num / den for x >= 0 without overflowing 64 bits (x up to days of 10 MHz ticks). */
static int64_t scale(int64_t x, int64_t num, int64_t den) {
    if (x <= 0 || den <= 0) return 0;
    return (x / den) * num + ((x % den) * num) / den;
}

int vc_timing(double rate, int64_t real_freq, int64_t *ticks_per_frame, int64_t *vfreq) {
    if (!(rate > 0.0) || rate > 100000.0 || real_freq <= 0) return 0;
    int64_t tpf = (int64_t)((double)real_freq / rate + 0.5);
    if (tpf < 1) tpf = 1;
    int64_t f = (int64_t)((double)tpf * rate + 0.5);
    if (f < 1) f = 1;
    *ticks_per_frame = tpf;
    *vfreq = f;
    return 1;
}

int vc_init(vc_clock *c, double rate, int64_t real_freq, uint32_t creep_after, uint32_t stuck_ms,
            vc_real_now_fn real_now, void *ctx) {
    memset(c, 0, sizeof *c);
    if (!real_now || !vc_timing(rate, real_freq, &c->ticks_per_frame, &c->vfreq)) return 0;
    c->rate = rate;
    c->real_freq = real_freq;
    c->creep_per_query = c->ticks_per_frame / CREEP_DIV > 0 ? c->ticks_per_frame / CREEP_DIV : 1;
    c->creep_after = creep_after ? creep_after : 4096;
    c->stuck_ms = stuck_ms ? stuck_ms : 1000;
    c->stuck_real = scale(c->stuck_ms, real_freq, 1000);
    c->real_now = real_now;
    c->ctx = ctx;
    c->real_origin = real_now(ctx);
    return 1;
}

/* Real ticks since vc_init, in virtual ticks. */
static int64_t passthrough(const vc_clock *c, int64_t real) {
    return scale(real - c->real_origin, c->vfreq, c->real_freq);
}

/* vnow = max(vnow, want), atomically; returns the new value. Virtual time never moves back: a
 * game subtracting two counter values would see a huge (unsigned) or negative delta. */
static int64_t advance(vc_clock *c, int64_t want) {
    int64_t cur = LOAD(&c->vnow);
    while (want > cur) {
        if (__atomic_compare_exchange_n(&c->vnow, &cur, want, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) return want;
    }
    return cur;
}

int64_t vc_now(vc_clock *c) {
    if (!LOAD(&c->engaged)) {
        /* Pass-through until the first Present, monotonic all the same. */
        return advance(c, passthrough(c, c->real_now(c->ctx)));
    }
    uint64_t q = ADD(&c->queries, 1);
    ADD(&c->queries_total, 1);
    int64_t base = LOAD(&c->frame_base);
    int64_t want = base;
    if (q > c->creep_after) {
        uint64_t steps = q - c->creep_after;
        if (steps > (1ull << 40)) steps = 1ull << 40;
        want = base + (int64_t)steps * c->creep_per_query;
    }
    /* The escape: real time is read for the first 64 queries of a frame and every 64th
     * after, so a frame that queries rarely (a loading thread) is still released on time and
     * a spinning one costs a real read only now and then. */
    if (q <= 64 || (q & 63) == 0) {
        int64_t elapsed = c->real_now(c->ctx) - LOAD(&c->real_frame_start);
        if (elapsed > c->stuck_real) {
            int64_t esc = base + scale(elapsed - c->stuck_real, c->vfreq, c->real_freq);
            if (esc > want) {
                want = esc;
                uint32_t zero = 0;
                if (__atomic_compare_exchange_n(&c->frame_escaped, &zero, 1, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
                    ADD(&c->escapes, 1);
            }
        }
    }
    return advance(c, want);
}

void vc_present(vc_clock *c) {
    int64_t real = c->real_now(c->ctx);
    if (!LOAD(&c->engaged)) {
        /* Engage at the next whole virtual second after where pass-through had it: forward
         * only, and the same phase in every run, so millisecond clocks (which floor) see the
         * same values run after run however long start-up really took. */
        int64_t p = passthrough(c, real);
        int64_t t = advance(c, (p / c->vfreq + 1) * c->vfreq);
        STORE(&c->frame_base, t);
        STORE(&c->real_frame_start, real);
        STORE(&c->engaged_real, real);
        STORE(&c->engaged_v, t);
        STORE(&c->queries, 0);
        STORE(&c->engaged, 1);
        return;
    }
    uint64_t q = XCHG(&c->queries, 0);
    uint64_t worst = LOAD(&c->queries_max);
    while (q > worst && !__atomic_compare_exchange_n(&c->queries_max, &worst, q, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
    }
    if (q > c->creep_after) ADD(&c->creep_frames, 1);
    STORE(&c->frame_escaped, 0);
    int64_t grid = LOAD(&c->frame_base) + c->ticks_per_frame;
    int64_t next = advance(c, grid);
    if (next != grid) ADD(&c->overruns, 1);
    STORE(&c->frame_base, next);
    STORE(&c->real_frame_start, real);
    ADD(&c->frames, 1);
}

uint32_t vc_sleep(vc_clock *c, uint32_t requested, uint32_t max_ms) {
    if (!LOAD(&c->engaged)) return requested;
    uint32_t real = requested < max_ms ? requested : max_ms;
    ADD(&c->sleeps, 1);
    ADD(&c->sleep_ms_requested, (uint64_t)requested);
    ADD(&c->sleep_ms_slept, (uint64_t)real);
    return real;
}

uint64_t vc_ticks_ms(const vc_clock *c, int64_t ticks) { return (uint64_t)scale(ticks, 1000, c->vfreq); }

uint64_t vc_ticks_100ns(const vc_clock *c, int64_t ticks) { return (uint64_t)scale(ticks, 10000000, c->vfreq); }

double vc_virtual_seconds(vc_clock *c) {
    if (!LOAD(&c->engaged)) return 0.0;
    return (double)(LOAD(&c->vnow) - LOAD(&c->engaged_v)) / (double)c->vfreq;
}

double vc_real_seconds(vc_clock *c) {
    if (!LOAD(&c->engaged)) return 0.0;
    return (double)(c->real_now(c->ctx) - LOAD(&c->engaged_real)) / (double)c->real_freq;
}
