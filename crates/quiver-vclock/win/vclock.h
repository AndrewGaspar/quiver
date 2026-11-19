/* The virtual clock's arithmetic, with no Windows in it: the hook DLL (hook.c) runs it
 * in the game, and the crate's unit tests run the same file on the host (src/lib.rs).
 *
 * The game is told every frame took exactly 1/rate seconds: virtual time advances by a whole
 * number of ticks per Present. Before the first Present the clock passes real time through,
 * so start-up behaves as it always did; the first Present moves it on to the next whole
 * second, so every run's clock has the same phase from then on. Three safety valves keep a game that waits on the
 * clock from hanging (each counted, so "it never fired" is a measurement):
 *
 * - sub-frame creep: after `creep_after` time queries in one frame (a frame can only be
 *   spinning on the clock by then), each further query moves time on by ticks_per_frame/1024;
 * - a real-time escape: once a frame has taken `stuck_ms` of real time, virtual time follows
 *   real time (minus stuck_ms) until the next Present;
 * - time never goes backwards: a Present moves time to max(grid, now) and counts the frames
 *   where the creep or the escape had already passed the grid (overruns).
 *
 * A Sleep never advances virtual time; it only gets shorter in real time (vc_sleep).
 *
 * Prior art: the virtual clock of the ds-decomp project's RIG shim (docs/17-virtual-clock.md). */
#ifndef QUIVER_VCLOCK_H
#define QUIVER_VCLOCK_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int64_t (*vc_real_now_fn)(void *ctx);

typedef struct vc_clock {
    /* Fixed by vc_init. */
    double rate;               /* virtual frames per second */
    int64_t real_freq;         /* the real performance counter's frequency */
    int64_t ticks_per_frame;   /* whole ticks, so every frame delta is identical */
    int64_t vfreq;             /* what QueryPerformanceFrequency reports: ticks_per_frame * rate */
    int64_t creep_per_query;
    int64_t stuck_real;        /* stuck_ms in real ticks */
    uint64_t creep_after;
    uint32_t stuck_ms;
    int64_t real_origin;       /* the real counter at vc_init */
    vc_real_now_fn real_now;
    void *ctx;

    /* Live (atomics). */
    int32_t engaged;
    int64_t vnow;              /* virtual ticks since vc_init, never decreasing */
    int64_t frame_base;        /* virtual ticks at the last Present */
    int64_t real_frame_start;  /* real counter at the last Present */
    uint64_t queries;          /* time queries in the current frame */
    uint32_t frame_escaped;    /* the current frame used the escape */
    int64_t engaged_real;      /* real counter at engagement */
    int64_t engaged_v;         /* virtual ticks at engagement */

    /* Counters. */
    uint64_t frames;           /* Presents since engagement */
    uint64_t queries_total;
    uint64_t queries_max;      /* the most queries one frame made */
    uint64_t creep_frames;     /* frames in which the creep moved time */
    uint64_t escapes;          /* frames released by the real-time escape */
    uint64_t overruns;         /* Presents where time was already past the grid */
    uint64_t sleeps;
    uint64_t sleep_ms_requested;
    uint64_t sleep_ms_slept;
} vc_clock;

/* Whole ticks per frame for `rate` on a counter of `real_freq`, and the frequency to report
 * (`ticks_per_frame * rate`, rounded): a few ppm from the real one, and exactly 1/rate per
 * frame when that product is a whole number. Returns 0 for a rate that can't work. */
int vc_timing(double rate, int64_t real_freq, int64_t *ticks_per_frame, int64_t *vfreq);

/* Set up `c` (zeroing it). `real_now` reads the real counter. */
int vc_init(vc_clock *c, double rate, int64_t real_freq, uint32_t creep_after, uint32_t stuck_ms,
            vc_real_now_fn real_now, void *ctx);

/* One time query from the game: virtual ticks since vc_init (real time scaled to vfreq before
 * the first Present). */
int64_t vc_now(vc_clock *c);

/* A frame boundary: the game presented. The first one engages the clock. */
void vc_present(vc_clock *c);

/* How long a Sleep(requested) really sleeps: as asked before engagement, at most `max_ms`
 * after. Virtual time never moves for a Sleep. */
uint32_t vc_sleep(vc_clock *c, uint32_t requested, uint32_t max_ms);

/* Virtual ticks as milliseconds and as 100 ns units (FILETIME), without overflow. */
uint64_t vc_ticks_ms(const vc_clock *c, int64_t ticks);
uint64_t vc_ticks_100ns(const vc_clock *c, int64_t ticks);

/* Since engagement (0 before): virtual and real seconds. */
double vc_virtual_seconds(vc_clock *c);
double vc_real_seconds(vc_clock *c);

/* Atomic reads of a counter (for reports). */
uint64_t vc_load_u64(const uint64_t *p);
int64_t vc_load_i64(const int64_t *p);

#ifdef __cplusplus
}
#endif

#endif
