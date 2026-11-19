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

int64_t vc_now(vc_clock *c, vc_thread *t) {
    if (LOAD(&c->held) > 0) {
        /* Held at a Present (pause or step): time is frozen. The reads count for nothing: a
         * thread polling the clock while the game is held isn't spinning on a frame, and the
         * held real time isn't a stuck frame. */
        ADD(&c->held_reads, 1);
        return LOAD(&c->vnow);
    }
    if (!LOAD(&c->engaged)) {
        /* Pass-through until the first Present, monotonic all the same. */
        return advance(c, passthrough(c, c->real_now(c->ctx)));
    }
    /* This thread's reads in this frame: the creep is for a thread spinning on the clock, not
     * for many threads that each read it now and then. */
    uint64_t gen = LOAD(&c->gen) + 1;
    if (t->frame != gen) {
        t->frame = gen;
        t->reads = 0;
    }
    uint64_t q = ++t->reads;
    ADD(&c->queries_total, 1);
    uint64_t most = LOAD(&c->frame_max_reads);
    while (q > most && !__atomic_compare_exchange_n(&c->frame_max_reads, &most, q, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
    }
    int64_t base = LOAD(&c->frame_base);
    int64_t want = base;
    if (q > c->creep_after) {
        uint64_t steps = q - c->creep_after;
        if (steps > (1ull << 40)) steps = 1ull << 40;
        want = base + (int64_t)steps * c->creep_per_query;
    }
    /* The escape: real time is read for a thread's first 64 reads of a frame and every 64th
     * after, so a frame that reads rarely (a loading thread) is still released on time and a
     * spinning one costs a real read only now and then. */
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
        STORE(&c->frame_max_reads, 0);
        ADD(&c->gen, 1);
        STORE(&c->engaged, 1);
        return;
    }
    uint64_t q = XCHG(&c->frame_max_reads, 0);
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
    ADD(&c->gen, 1);
    ADD(&c->frames, 1);
    if (LOAD(&c->hold)) ADD(&c->stepped, 1);
}

uint64_t vc_hold_cmd(vc_clock *c, int cmd, uint64_t arg, uint32_t timeout_ms) {
    uint64_t frames = LOAD(&c->frames);
    int on = LOAD(&c->hold);
    if (timeout_ms && (cmd == VC_CMD_PAUSE || cmd == VC_CMD_STEP)) STORE(&c->hold_timeout_real, scale(timeout_ms, c->real_freq, 1000));
    else if (cmd == VC_CMD_PAUSE || cmd == VC_CMD_STEP) STORE(&c->hold_timeout_real, 0);
    switch (cmd) {
    case VC_CMD_PAUSE:
        /* As soon as possible: the end of the Present in flight (or now, if one is held). */
        if (!on) {
            STORE(&c->hold_at, frames + 1);
            ADD(&c->pauses, 1);
            STORE(&c->hold, 1);
        } else if (LOAD(&c->hold_at) > frames + 1) {
            STORE(&c->hold_at, frames + 1); /* a step in progress stops early */
        }
        return LOAD(&c->hold_at);
    case VC_CMD_STEP: {
        if (arg < 1) arg = 1;
        ADD(&c->steps, arg);
        if (!on) {
            /* From running: the frame in flight is the first of the K. */
            STORE(&c->hold_at, frames + arg);
            ADD(&c->pauses, 1);
            STORE(&c->hold, 1);
            return frames + arg;
        }
        uint64_t from = LOAD(&c->hold_at);
        if (from < frames) from = frames;
        STORE(&c->hold_at, from + arg);
        return from + arg;
    }
    case VC_CMD_RESUME:
        STORE(&c->hold, 0);
        STORE(&c->hold_at, 0);
        return 0;
    default:
        return on ? LOAD(&c->hold_at) : 0;
    }
}

int vc_must_hold(vc_clock *c) {
    return LOAD(&c->hold) && LOAD(&c->frames) >= LOAD(&c->hold_at);
}

void vc_hold_begin(vc_clock *c) {
    int64_t real = c->real_now(c->ctx);
    if (ADD(&c->held, 1) == 1) {
        STORE(&c->held_since, real);
        STORE(&c->held_v, LOAD(&c->vnow));
        ADD(&c->holds, 1);
    }
}

void vc_hold_end(vc_clock *c) {
    int64_t real = c->real_now(c->ctx);
    if (ADD(&c->held, -1) == 0) {
        ADD(&c->leaked, LOAD(&c->vnow) - LOAD(&c->held_v));
        ADD(&c->held_real, real - LOAD(&c->held_since));
    }
    /* The frame's real time starts now: the escape measures the frame, not the hold. */
    STORE(&c->real_frame_start, real);
}

int vc_hold_expired(vc_clock *c) {
    int64_t t = LOAD(&c->hold_timeout_real);
    if (!t || LOAD(&c->held) <= 0 || !LOAD(&c->hold)) return 0;
    if (c->real_now(c->ctx) - LOAD(&c->held_since) < t) return 0;
    STORE(&c->hold, 0);
    STORE(&c->hold_at, 0);
    ADD(&c->auto_resumes, 1);
    return 1;
}

void vc_ctl_attach(vc_ctl *k, vc_clock *c, uint64_t pid) {
    STORE(&k->magic, VC_CTL_MAGIC);
    STORE(&k->version, (uint64_t)VC_CTL_VERSION);
    STORE(&k->pid, pid);
    /* Commands written before this process claimed the page were for another one. */
    STORE(&k->ack_seq, LOAD(&k->cmd_seq));
    vc_ctl_publish(k, c);
}

int vc_ctl_poll(vc_ctl *k, vc_clock *c, uint64_t *cmd_out, uint64_t *arg_out) {
    uint64_t seq = LOAD(&k->cmd_seq);
    if (seq == LOAD(&k->ack_seq)) return 0;
    uint64_t cmd = LOAD(&k->cmd), arg = LOAD(&k->cmd_arg), to = LOAD(&k->cmd_timeout_ms);
    uint64_t frames = LOAD(&c->frames);
    uint64_t at = vc_hold_cmd(c, (int)cmd, arg, to > 0xffffffffu ? 0xffffffffu : (uint32_t)to);
    STORE(&k->ack_frames, frames);
    STORE(&k->ack_hold_at, at);
    STORE(&k->ack_cmd, cmd);
    vc_ctl_publish(k, c);
    STORE(&k->ack_seq, seq);
    if (cmd_out) *cmd_out = cmd;
    if (arg_out) *arg_out = arg;
    return 1;
}

void vc_ctl_publish(vc_ctl *k, vc_clock *c) {
    while (XCHG(&c->pub_lock, 1)) {
    }
    uint64_t u = LOAD(&k->update);
    STORE(&k->update, u | 1); /* odd: writing */
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    int held = LOAD(&c->held) > 0, on = LOAD(&c->hold);
    int engaged = LOAD(&c->engaged);
    STORE(&k->state, (uint64_t)(held ? VC_STATE_HELD : on ? VC_STATE_HOLDING : VC_STATE_RUNNING));
    STORE(&k->engaged, (uint64_t)engaged);
    STORE(&k->frames, LOAD(&c->frames));
    STORE(&k->vticks, engaged ? LOAD(&c->vnow) - LOAD(&c->engaged_v) : 0);
    STORE(&k->hold_at, on ? LOAD(&c->hold_at) : 0);
    STORE(&k->ticks_per_frame, (uint64_t)c->ticks_per_frame);
    STORE(&k->qpf, (uint64_t)c->vfreq);
    STORE(&k->pauses, LOAD(&c->pauses));
    STORE(&k->steps, LOAD(&c->steps));
    STORE(&k->stepped, LOAD(&c->stepped));
    STORE(&k->holds, LOAD(&c->holds));
    int64_t hr = LOAD(&c->held_real);
    if (held) hr += c->real_now(c->ctx) - LOAD(&c->held_since);
    STORE(&k->held_us, (uint64_t)scale(hr, 1000000, c->real_freq));
    STORE(&k->held_reads, LOAD(&c->held_reads));
    STORE(&k->leaked, LOAD(&c->leaked));
    STORE(&k->auto_resumes, LOAD(&c->auto_resumes));
    STORE(&k->creep_frames, LOAD(&c->creep_frames));
    STORE(&k->escapes, LOAD(&c->escapes));
    STORE(&k->overruns, LOAD(&c->overruns));
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    STORE(&k->update, (u | 1) + 1); /* even: done */
    STORE(&c->pub_lock, 0);
}

uint32_t vc_sleep(vc_clock *c, uint32_t requested, uint32_t max_ms, int shorten) {
    if (!LOAD(&c->engaged)) return requested;
    if (!shorten) {
        ADD(&c->sleeps_real, 1);
        return requested;
    }
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
