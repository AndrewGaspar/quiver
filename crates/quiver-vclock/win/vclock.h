/* The virtual clock's arithmetic, with no Windows in it: the hook DLL (hook.c) runs it in the
 * game, and the crate's unit tests run the same file on the host (src/lib.rs).
 *
 * The game is told every frame took exactly 1/rate seconds: virtual time advances by a whole
 * number of ticks per Present. Before the first Present the clock passes real time through,
 * so start-up behaves as it always did; the first Present moves it on to the next whole
 * second, so every run's clock has the same phase from then on. Three safety valves keep a
 * game that waits on the clock from hanging (each counted, so "it never fired" is a
 * measurement):
 *
 * - sub-frame creep: once ONE THREAD has read the clock `creep_after` times in one frame (it
 *   can only be spinning on the clock by then), each further read by that thread moves time
 *   on by ticks_per_frame/1024. Reads are counted per thread: many threads each reading a
 *   little (job systems, timer threads) don't add up to a spin;
 * - a real-time escape: once a frame has taken `stuck_ms` of real time, virtual time follows
 *   real time (minus stuck_ms) until the next Present. A frame released this way tracked the
 *   machine, so it isn't repeatable;
 * - time never goes backwards: a Present moves time to max(grid, now) and counts the frames
 *   where the creep or the escape had already passed the grid (overruns).
 *
 * A Sleep never advances virtual time. The caller decides which sleeps get shorter in real
 * time (vc_sleep's `shorten`): the DLL shortens only the presenting thread's, so a frame
 * limiter that sleeps terminates fast while background threads that sleep-poll keep their
 * real sleeps and don't turn into clock-read storms.
 *
 * Pause and step (`quiver lab clock N pause|step K|resume`): a hold stops the game at its
 * Present, after the real Present returned and the clock moved on a frame, so the frame it just
 * presented stays on screen. While a thread is held there, virtual time doesn't move: reads
 * return the frozen value without counting towards the creep, and the real-time escape stands
 * down (a frame's real time starts again when it is released). A step lets K more Presents
 * through, each the usual exactly-one-frame advance, then holds again, so a stepped run sees the
 * same virtual-time sequence as a free-running one. Whatever still moved while held is counted
 * (`leaked`; 0 by construction unless a race slipped through). The controller talks to the
 * clock through `vc_ctl`, a page both sides map (vclock.ctl in the report folder).
 *
 * Prior art: the virtual clock of the user's Ishimura project (ds-decomp's RIG shim,
 * docs/17-virtual-clock.md). */
#ifndef QUIVER_VCLOCK_H
#define QUIVER_VCLOCK_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int64_t (*vc_real_now_fn)(void *ctx);

/* One thread's reads in the current frame (zero-initialised; the caller keeps one per thread). */
typedef struct vc_thread {
    uint64_t frame; /* the frame (gen + 1) `reads` belongs to; 0: none yet */
    uint64_t reads;
} vc_thread;

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
    uint64_t gen;              /* frame generation: +1 at every Present (and engagement) */
    uint64_t frame_max_reads;  /* the most reads one thread made in the current frame */
    uint32_t frame_escaped;    /* the current frame used the escape */
    int64_t engaged_real;      /* real counter at engagement */
    int64_t engaged_v;         /* virtual ticks at engagement */

    /* Counters. */
    uint64_t frames;           /* Presents since engagement */
    uint64_t queries_total;
    uint64_t queries_max;      /* the most reads one thread made in one frame */
    uint64_t creep_frames;     /* frames in which the creep moved time */
    uint64_t escapes;          /* frames released by the real-time escape */
    uint64_t overruns;         /* Presents where time was already past the grid */
    uint64_t sleeps;           /* sleeps shortened */
    uint64_t sleep_ms_requested;
    uint64_t sleep_ms_slept;
    uint64_t sleeps_real;      /* sleeps left as asked (not the presenting thread's) */

    /* Pause and step (live, atomics). */
    int32_t hold;              /* 1: presents stop once `frames` reaches `hold_at` */
    int32_t held;              /* threads blocked at the gate now */
    uint64_t hold_at;          /* the present count to stop at */
    int64_t hold_timeout_real; /* resume by itself after this long held (real ticks); 0 never */
    int64_t held_since;        /* real counter when the first thread blocked */
    int64_t held_v;            /* vnow then */
    int32_t pub_lock;          /* vc_ctl_publish's spinlock */
    int32_t pad0;

    /* Pause and step counters. */
    uint64_t pauses;           /* holds started (a pause, or a step from running) */
    uint64_t steps;            /* frames asked for by step commands */
    uint64_t stepped;          /* presents while a hold was on (steps let through) */
    uint64_t holds;            /* times the gate blocked */
    int64_t held_real;         /* real ticks spent held, in all */
    uint64_t held_reads;       /* clock reads while held: frozen, no creep, no escape */
    int64_t leaked;            /* virtual ticks time moved while held (0 by design) */
    uint64_t auto_resumes;     /* holds ended by their timeout, not by the controller */
} vc_clock;

/* Commands (vc_hold_cmd, vc_ctl.cmd). */
enum { VC_CMD_NONE = 0, VC_CMD_PAUSE = 1, VC_CMD_STEP = 2, VC_CMD_RESUME = 3 };
/* vc_ctl.state. */
enum { VC_STATE_RUNNING = 0, VC_STATE_HOLDING = 1, VC_STATE_HELD = 2 };

/* The control page (vclock.ctl, VC_CTL_SIZE bytes, mapped shared by the DLL and the lab).
 * Every field is 64 bits and naturally aligned. The controller writes cmd, cmd_arg and
 * cmd_timeout_ms, then cmd_seq + 1 (release); the clock applies a command once
 * (vc_ctl_poll), publishes its state, then sets ack_seq. The state block is a seqlock:
 * `update` is odd while the clock writes it. src/lib.rs mirrors this layout. */
#define VC_CTL_MAGIC 0x314c544356515551ull /* "QUQVCTL1" */
#define VC_CTL_VERSION 1
#define VC_CTL_SIZE 4096
typedef struct vc_ctl {
    uint64_t magic, version, pid;
    /* The controller's. */
    uint64_t cmd_seq, cmd, cmd_arg, cmd_timeout_ms;
    /* The clock's. */
    uint64_t ack_seq;
    uint64_t update;
    uint64_t state;            /* VC_STATE_* */
    uint64_t engaged;
    uint64_t frames;           /* presents counted since engagement */
    int64_t vticks;            /* virtual ticks since engagement */
    uint64_t hold_at;
    uint64_t ticks_per_frame;
    uint64_t qpf;
    uint64_t pauses, steps, stepped, holds;
    uint64_t held_us;          /* real time held, in all */
    uint64_t held_reads;
    int64_t leaked;
    uint64_t auto_resumes;
    uint64_t ack_frames;       /* frames when the last command was applied */
    uint64_t ack_hold_at;      /* hold_at it set (a step's target; 0 after resume) */
    uint64_t ack_cmd;          /* the last command applied */
    uint64_t creep_frames, escapes, overruns; /* the valves, for the controller's report */
} vc_ctl;

/* Whole ticks per frame for `rate` on a counter of `real_freq`, and the frequency to report
 * (`ticks_per_frame * rate`, rounded): a few ppm from the real one, and exactly 1/rate per
 * frame when that product is a whole number. Returns 0 for a rate that can't work. */
int vc_timing(double rate, int64_t real_freq, int64_t *ticks_per_frame, int64_t *vfreq);

/* Set up `c` (zeroing it). `real_now` reads the real counter. */
int vc_init(vc_clock *c, double rate, int64_t real_freq, uint32_t creep_after, uint32_t stuck_ms,
            vc_real_now_fn real_now, void *ctx);

/* One time read by the game on thread `t`: virtual ticks since vc_init (real time scaled to
 * vfreq before the first Present). */
int64_t vc_now(vc_clock *c, vc_thread *t);

/* A frame boundary: the game presented. The first one engages the clock. */
void vc_present(vc_clock *c);

/* How long a Sleep(requested) really sleeps: as asked before engagement or when `shorten` is
 * 0, else at most `max_ms`. Virtual time never moves for a Sleep. */
uint32_t vc_sleep(vc_clock *c, uint32_t requested, uint32_t max_ms, int shorten);

/* Pause and step. A pause holds at the end of the next Present (the frame in flight finishes);
 * a step of K holds K Presents after the current one (from running, it is a pause that lets K
 * more through first); a resume releases. Returns the present count the clock will hold at
 * (0 after a resume). `timeout_ms` (pause and step): resume by itself after that long held. */
uint64_t vc_hold_cmd(vc_clock *c, int cmd, uint64_t arg, uint32_t timeout_ms);
/* Must the presenting thread block now (a hold is on and `frames` reached `hold_at`)? */
int vc_must_hold(vc_clock *c);
/* A thread blocks at the gate / is released (its frame's real time starts again). */
void vc_hold_begin(vc_clock *c);
void vc_hold_end(vc_clock *c);
/* The hold's timeout passed: the hold is lifted (counted). Returns 1 then. */
int vc_hold_expired(vc_clock *c);

/* The control page: claim it for this process (stale commands ignored), apply a pending
 * command (returns 1 and the command, else 0), publish the clock's state. */
void vc_ctl_attach(vc_ctl *k, vc_clock *c, uint64_t pid);
int vc_ctl_poll(vc_ctl *k, vc_clock *c, uint64_t *cmd, uint64_t *arg);
void vc_ctl_publish(vc_ctl *k, vc_clock *c);

/* The vtable hook registry (hook.c's Present and CreateSwapChain hooks), here so the host tests
 * run its decisions. One entry per (vtable, slot): the hook we put there and `orig`, what the
 * slot held before it, which the hook calls (vc_vt_orig). Vtables are per class, so every
 * object of a class shares one, and a hook is asked for again for each new object.
 *
 * When a registered slot no longer holds our hook, vc_vt_plan decides:
 * - it holds `orig` again (someone unhooked down to the original): put ours back, same entry;
 * - it holds anything else: another module hooked the slot after us. It read the slot then,
 *   so the function it saved and calls is ours: it is in front of us in the chain and calls
 *   through us to `orig`. Leave it alone. Hooking over it again (the old behaviour) put ours
 *   in front with a second entry whose original vc_vt_orig never returned, so the other
 *   module's hook was skipped (Pharos's d3d11.dll proxy never saw the swap chain). Chaining
 *   onto it instead (ours -> theirs -> ours) would run ours twice per call, or recurse for
 *   ever with one original per slot, and the two hookers would take the slot back from each
 *   other for every new object of the class.
 * The caller holds a lock around vc_vt_plan; vc_vt_orig is lock-free (entries are appended
 * and never change after `n` is published). */
#define VC_MAX_VH 128
typedef struct vc_vhook {
    void **vt;
    void *hook;
    void *orig;
    uint32_t slot;
    uint32_t noted; /* the caller logged a foreign hook over this one */
} vc_vhook;
typedef struct vc_vtreg {
    vc_vhook e[VC_MAX_VH];
    int32_t n;
} vc_vtreg;
enum {
    VC_VT_NEW = 0,      /* registered (orig = the slot's value): write the hook */
    VC_VT_OURS = 1,     /* the slot holds our hook: nothing to do */
    VC_VT_WRAPPED = 2,  /* another module's hook over ours (which it calls): leave it */
    VC_VT_RESTORED = 3, /* the slot holds our original again: write the hook back */
    VC_VT_FULL = 4,     /* no room: left alone */
};
/* What to do about `hook` for (vt, slot), whose slot holds `cur` now. `entry` (may be NULL)
 * gets the registry entry, when there is one. */
int vc_vt_plan(vc_vtreg *r, void **vt, uint32_t slot, void *cur, void *hook, vc_vhook **entry);
/* What `hook`, called for an object whose vtable is `vt`, calls next: the entry's original.
 * An unregistered vtable (a copy of a hooked one, say) whose slot holds `hook` gets the
 * original of the entries for the same slot and hook when they all agree, else NULL. */
void *vc_vt_orig(const vc_vtreg *r, void **vt, uint32_t slot, void *hook);
/* sizeof(vc_vtreg), for the Rust mirror's test. */
uint64_t vc_vtreg_size(void);

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
