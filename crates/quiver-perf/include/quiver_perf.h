/* quiver_perf.h - Quiver's shared-memory performance formats (design/SIGHT-HUD.md §3.8).
 *
 * The ring (per-frame / per-event records from the Sight layer and from mods) and hud.image
 * (the HUD image quiver-sight renders and the layer composites) as C structs, plus a lock-free
 * ring writer for mods (quiver_mod.h builds on it). Single header, C99 or C++, GCC / Clang /
 * MinGW / MSVC, no dependencies. Little-endian x86 only (64-bit, and 32-bit for 32-bit
 * games: the layouts are the same, every 64-bit field sits at a multiple of 8).
 *
 * This file mirrors crates/quiver-perf/src/layout.rs. Every size and offset is asserted below;
 * crates/quiver-perf/tests/header.rs checks those asserts and every #define against the Rust
 * layout, and compiles this header so the C compiler checks the asserts against the structs.
 * Change both together.
 *
 * SPDX-License-Identifier: 0BSD
 */
#ifndef QUIVER_PERF_H
#define QUIVER_PERF_H

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_MSC_VER) && !defined(__cplusplus)
#define QV_INLINE static __inline
#else
#define QV_INLINE static inline
#endif

/* ---- Constants ---------------------------------------------------------------------- */

#define QV_RING_MAGIC             0x474E495256515551ull /* "QUQVRING" */
#define QV_RING_VERSION           1
#define QV_RING_HEADER_BYTES      4096
#define QV_SLOT_BYTES             128
#define QV_RING_MIN_SLOTS         16
#define QV_RING_MAX_SLOTS         8192 /* 1 MiB of slots */
#define QV_RING_DEFAULT_SLOTS     4096
#define QV_RING_STATUS_BYTES      2048
#define QV_RING_STATUS_DATA_BYTES 2032
#define QV_RECORD_FIELDS          12
#define QV_EVENT_LABEL_BYTES      48

#define QV_CLOCK_QPC              1 /* QueryPerformanceCounter ticks (Wine: CLOCK_MONOTONIC_RAW / 100) */
#define QV_CLOCK_MONO_RAW_NS      2 /* CLOCK_MONOTONIC_RAW nanoseconds */
#define QV_PRODUCER_LAYER         1
#define QV_PRODUCER_MOD           2

/* qv_record.flags. Frames: REAL, GENERATED, or REAL|NOFG (FG off). Events: HAS_LABEL,
 * HAS_DURATION. Layer presents: PRESENT_ID, PRESENT_HUD. */
#define QV_FLAG_REAL              0x01
#define QV_FLAG_GENERATED         0x02
#define QV_FLAG_NOFG              0x04
#define QV_FLAG_HAS_LABEL         0x08
#define QV_FLAG_HAS_DURATION      0x10
#define QV_FLAG_PRESENT_ID        0x20 /* key is the app's VkPresentIdKHR (DXVK frame id) */
#define QV_FLAG_PRESENT_HUD       0x40 /* the HUD was composited into this present */

/* Metric index 0 is never valid. Mods get 1..64 from metrics_ok. */
#define QV_METRIC_NONE            0
#define QV_LAYER_METRIC_PRESENT   1

/* Layer present record fields (metric QV_LAYER_METRIC_PRESENT, "present.frame"). */
#define QV_PRESENT_F_ENTRY        0 /* CLOCK_MONOTONIC_RAW ns at vkQueuePresentKHR entry (= t) */
#define QV_PRESENT_F_RETURN       1 /* ... when the driver's present returned */
#define QV_PRESENT_F_IMAGE_INDEX  2
#define QV_PRESENT_F_PRESENT_MODE 3 /* VkPresentModeKHR */
#define QV_PRESENT_F_RESULT       4 /* VkResult for this swapchain */
#define QV_PRESENT_F_PRESENT_ID   5 /* VkPresentIdKHR id, 0 if none */
#define QV_PRESENT_F_SEQ          6 /* the layer's own per-swapchain count */
#define QV_PRESENT_F_HUD_NS       7 /* CPU ns spent on the HUD in this present */
#define QV_PRESENT_NFIELDS        8

#define QV_STATUS_FORMAT_NONE     0
#define QV_STATUS_FORMAT_LAYER    1 /* status data holds a qv_layer_status */

#define QV_LAYER_DRAWING          0x01
#define QV_LAYER_LOW_LATENCY2     0x02
#define QV_LAYER_PRESENT_ID       0x04
#define QV_LAYER_PRESENT_WAIT     0x08
#define QV_LAYER_PANICKED         0x10
#define QV_LAYER_HUD_OFF          0x20
#define QV_LAYER_WAYLAND_SURFACE  0x40 /* the game made a Wayland VkSurfaceKHR */
#define QV_LAYER_X11_SURFACE      0x80 /* ... an Xlib or XCB one */

/* qv_layer_status.keys: the layer's in-game HUD key polling (X11 XQueryKeymap). */
#define QV_KEYS_UNKNOWN           0 /* not reported (not started, or an older layer) */
#define QV_KEYS_ACTIVE            1
#define QV_KEYS_OFF               2 /* QUIVER_SIGHT_KEYS=0 */
#define QV_KEYS_NO_DISPLAY        3
#define QV_KEYS_NO_LIBX11         4
#define QV_KEYS_OPEN_FAILED       5
#define QV_KEYS_NO_KEYCODES       6
#define QV_KEYS_IO_ERROR          7
#define QV_KEYS_INTERNAL          8

#define QV_DRAW_ERR_NONE          0
#define QV_DRAW_ERR_FORMAT        1
#define QV_DRAW_ERR_USAGE         2
#define QV_DRAW_ERR_QUEUE         3
#define QV_DRAW_ERR_DEVICE_LOST   4
#define QV_DRAW_ERR_NO_IMAGE      5
#define QV_DRAW_ERR_INTERNAL      6

#define QV_HUD_MAGIC              0x4944554856515551ull /* "QUQVHUDI" */
#define QV_HUD_VERSION            1
#define QV_HUD_HEADER_BYTES       4096
#define QV_HUD_PIXELS_OFFSET      256
#define QV_HUD_MAX_BUFFER_BYTES   4194304
#define QV_HUD_MAX_DIM            2048
#define QV_HUD_MAX_GRAPHS         2
#define QV_HUD_FORMAT_RGBA8_PREMUL_SRGB 1

#define QV_GRAPH_SERIES_PRESENTED      1
#define QV_GRAPH_SERIES_BASE           2
#define QV_GRAPH_SERIES_GENERATED_TINT 4

/* High bit of qv_record.commit: the slot is being written for index (commit & ~BUSY) - 1. */
#define QV_COMMIT_BUSY            0x8000000000000000ull

/* ---- Ring -------------------------------------------------------------------------- */

/* The ring header: one 4 KiB page at offset 0 of the file; slots follow at
 * QV_RING_HEADER_BYTES. Fields marked [w] change while the ring is live; the rest are fixed
 * at creation, and magic is written last (release). */
typedef struct qv_status_block {
    uint64_t seq;               /* [w] seqlock: odd while the writer updates the block */
    uint32_t len;               /* [w] valid bytes in data */
    uint32_t format;            /* [w] QV_STATUS_FORMAT_* */
    uint8_t  data[QV_RING_STATUS_DATA_BYTES];
} qv_status_block;

typedef struct qv_ring_header {
    uint64_t magic;
    uint16_t version;
    uint16_t header_bytes;      /* 4096 */
    uint16_t slot_bytes;        /* 128 */
    uint16_t clock;             /* QV_CLOCK_* */
    uint32_t slot_count;        /* power of two, 16..8192 */
    uint32_t producer_kind;     /* QV_PRODUCER_* */
    uint64_t clock_freq;        /* ticks per second (QPC under Wine: 10,000,000) */
    uint8_t  nonce[16];         /* written by the creator, reported out of band */
    uint32_t writer_pid;        /* Linux pid for the layer; Windows pid for mods */
    uint32_t schema_rev;        /* [w] bumps when the producer's metric indices change */
    uint8_t  _pad0[8];
    uint64_t reserve;           /* [w] next slot index to reserve (writers: fetch_add); own line */
    uint8_t  _pad1[56];
    uint64_t heartbeat;         /* [w] writer's clock, >= 1/s even when idle */
    uint64_t dropped;           /* [w] records the writer discarded */
    uint8_t  _pad2[48];
    qv_status_block status;     /* [w] producer status (the layer: qv_layer_status) */
    uint8_t  _pad3[1856];
} qv_ring_header;

/* One 128-byte slot. */
typedef struct qv_record {
    uint64_t commit;            /* index + 1 once committed (release); see QV_COMMIT_BUSY */
    uint16_t metric;            /* index from metrics_ok (or QV_LAYER_METRIC_PRESENT); 0 invalid */
    uint8_t  flags;             /* QV_FLAG_* */
    uint8_t  nfields;           /* valid entries in f, <= 12 */
    uint32_t swapchain;         /* writer-chosen id; change it when the swapchain is recreated */
    uint64_t key;               /* correlation key: present seq, frame id, present id */
    int64_t  t;                 /* the record's own time, in the ring's clock */
    int64_t  f[QV_RECORD_FIELDS]; /* frame fields; series: f[0] = double bits;
                                     event: f[0] = duration, f[1..6] = 48 label bytes */
} qv_record;

/* The Sight layer's status block contents (format QV_STATUS_FORMAT_LAYER). */
typedef struct qv_layer_status {
    uint32_t struct_bytes;      /* sizeof(qv_layer_status) as the writer knew it */
    uint32_t flags;             /* QV_LAYER_* */
    uint32_t extent_w, extent_h;/* the swapchain the HUD draws on */
    uint32_t vk_format, color_space, present_mode, image_count;
    uint32_t swapchain;         /* the layer's id for it */
    uint32_t draw_error;        /* QV_DRAW_ERR_* */
    uint64_t drawn;             /* presents the HUD was composited into */
    uint64_t presents;
    uint64_t hud_published;     /* hud.image publish counter last uploaded */
    uint32_t vendor_id, device_id, driver_version, api_version;
    uint32_t engine_version;    /* VkApplicationInfo */
    uint32_t swapchain_count;
    char     engine_name[32];   /* NUL-padded UTF-8 ("DXVK", "vkd3d") */
    char     app_name[64];
    char     device_name[64];
    uint32_t keys;              /* QV_KEYS_* */
    uint16_t key_toggles;       /* HUD toggle / next-preset chord presses, wrapping */
    uint16_t key_nexts;
} qv_layer_status;

QV_INLINE qv_record *qv_ring_slots(qv_ring_header *h)
{ return (qv_record *)((char *)h + QV_RING_HEADER_BYTES); }

/* ---- hud.image --------------------------------------------------------------------- */

typedef struct qv_hud_header {      /* offset 0, one 4 KiB page */
    uint64_t magic;                 /* written last at creation (release) */
    uint16_t version;
    uint16_t header_bytes;          /* 4096 */
    uint32_t buffer_bytes;          /* capacity of each buffer incl. descriptor; 4096-multiple, <= 4 MiB */
    uint8_t  _pad0[48];
    uint64_t published;             /* publish counter; buffer (published % 2) is current */
    uint64_t heartbeat;             /* quiver-sight, CLOCK_MONOTONIC_RAW ns, >= 1/s */
    uint32_t writer_pid;
    uint8_t  _pad1[4012];
} qv_hud_header;

typedef struct qv_hud_graph {       /* a rectangle the layer fills with the live graph */
    int32_t  x, y, w, h;            /* in image pixels */
    uint32_t span_ms;               /* 1000..10000 */
    float    min_ms, max_ms;        /* vertical range; max_ms = 0: auto */
    uint32_t line, fill, muted;     /* premultiplied RGBA8, bytes R,G,B,A in memory */
    uint32_t warn_above_ms, bad_above_ms, warn, bad;
    uint32_t series;                /* QV_GRAPH_SERIES_* */
} qv_hud_graph;

typedef struct qv_hud_buffer {      /* at header_bytes + i * buffer_bytes */
    uint64_t seq;                   /* odd while quiver-sight writes this buffer */
    uint32_t width, height, stride; /* pixels, pixels, bytes per row (multiple of 4) */
    uint32_t format;                /* QV_HUD_FORMAT_RGBA8_PREMUL_SRGB */
    float    scale;                 /* image pixels per logical px */
    uint32_t extent_w, extent_h;    /* the swapchain extent it was laid out for */
    int32_t  x, y;                  /* where it goes, in swapchain pixels */
    float    opacity;               /* applied by the layer */
    uint32_t ngraphs;               /* <= 2 */
    qv_hud_graph graph[QV_HUD_MAX_GRAPHS];
    uint32_t _pad0;
    uint64_t model_rev;             /* the HUD model revision this image shows */
    uint8_t  _reserved[72];
    /* pixels at QV_HUD_PIXELS_OFFSET */
} qv_hud_buffer;

QV_INLINE qv_hud_buffer *qv_hud_buffer_at(qv_hud_header *h, uint32_t buffer_bytes, uint64_t i)
{ return (qv_hud_buffer *)((char *)h + QV_HUD_HEADER_BYTES + (size_t)(i % 2) * buffer_bytes); }

QV_INLINE uint8_t *qv_hud_pixels(qv_hud_buffer *b)
{ return (uint8_t *)b + QV_HUD_PIXELS_OFFSET; }

/* ---- Layout asserts (checked by the compiler; parsed by tests/header.rs) ------------ */

#define QV__CAT2(a, b) a##b
#define QV__CAT(a, b) QV__CAT2(a, b)
#define QV_ASSERT_SIZE(T, n) \
    typedef char QV__CAT(qv__size_, T)[(sizeof(T) == (n)) ? 1 : -1]
#define QV_ASSERT_OFFSET(T, field, n) \
    typedef char QV__CAT(QV__CAT(qv__off_, T), QV__CAT(_, field))[(offsetof(T, field) == (n)) ? 1 : -1]

QV_ASSERT_SIZE(qv_ring_header, 4096);
QV_ASSERT_OFFSET(qv_ring_header, magic, 0);
QV_ASSERT_OFFSET(qv_ring_header, version, 8);
QV_ASSERT_OFFSET(qv_ring_header, header_bytes, 10);
QV_ASSERT_OFFSET(qv_ring_header, slot_bytes, 12);
QV_ASSERT_OFFSET(qv_ring_header, clock, 14);
QV_ASSERT_OFFSET(qv_ring_header, slot_count, 16);
QV_ASSERT_OFFSET(qv_ring_header, producer_kind, 20);
QV_ASSERT_OFFSET(qv_ring_header, clock_freq, 24);
QV_ASSERT_OFFSET(qv_ring_header, nonce, 32);
QV_ASSERT_OFFSET(qv_ring_header, writer_pid, 48);
QV_ASSERT_OFFSET(qv_ring_header, schema_rev, 52);
QV_ASSERT_OFFSET(qv_ring_header, reserve, 64);
QV_ASSERT_OFFSET(qv_ring_header, heartbeat, 128);
QV_ASSERT_OFFSET(qv_ring_header, dropped, 136);
QV_ASSERT_OFFSET(qv_ring_header, status, 192);
QV_ASSERT_SIZE(qv_status_block, 2048);
QV_ASSERT_OFFSET(qv_status_block, seq, 0);
QV_ASSERT_OFFSET(qv_status_block, len, 8);
QV_ASSERT_OFFSET(qv_status_block, format, 12);
QV_ASSERT_OFFSET(qv_status_block, data, 16);
QV_ASSERT_SIZE(qv_record, 128);
QV_ASSERT_OFFSET(qv_record, commit, 0);
QV_ASSERT_OFFSET(qv_record, metric, 8);
QV_ASSERT_OFFSET(qv_record, flags, 10);
QV_ASSERT_OFFSET(qv_record, nfields, 11);
QV_ASSERT_OFFSET(qv_record, swapchain, 12);
QV_ASSERT_OFFSET(qv_record, key, 16);
QV_ASSERT_OFFSET(qv_record, t, 24);
QV_ASSERT_OFFSET(qv_record, f, 32);
QV_ASSERT_SIZE(qv_layer_status, 256);
QV_ASSERT_OFFSET(qv_layer_status, struct_bytes, 0);
QV_ASSERT_OFFSET(qv_layer_status, flags, 4);
QV_ASSERT_OFFSET(qv_layer_status, extent_w, 8);
QV_ASSERT_OFFSET(qv_layer_status, extent_h, 12);
QV_ASSERT_OFFSET(qv_layer_status, vk_format, 16);
QV_ASSERT_OFFSET(qv_layer_status, color_space, 20);
QV_ASSERT_OFFSET(qv_layer_status, present_mode, 24);
QV_ASSERT_OFFSET(qv_layer_status, image_count, 28);
QV_ASSERT_OFFSET(qv_layer_status, swapchain, 32);
QV_ASSERT_OFFSET(qv_layer_status, draw_error, 36);
QV_ASSERT_OFFSET(qv_layer_status, drawn, 40);
QV_ASSERT_OFFSET(qv_layer_status, presents, 48);
QV_ASSERT_OFFSET(qv_layer_status, hud_published, 56);
QV_ASSERT_OFFSET(qv_layer_status, vendor_id, 64);
QV_ASSERT_OFFSET(qv_layer_status, device_id, 68);
QV_ASSERT_OFFSET(qv_layer_status, driver_version, 72);
QV_ASSERT_OFFSET(qv_layer_status, api_version, 76);
QV_ASSERT_OFFSET(qv_layer_status, engine_version, 80);
QV_ASSERT_OFFSET(qv_layer_status, swapchain_count, 84);
QV_ASSERT_OFFSET(qv_layer_status, engine_name, 88);
QV_ASSERT_OFFSET(qv_layer_status, app_name, 120);
QV_ASSERT_OFFSET(qv_layer_status, device_name, 184);
QV_ASSERT_OFFSET(qv_layer_status, keys, 248);
QV_ASSERT_OFFSET(qv_layer_status, key_toggles, 252);
QV_ASSERT_OFFSET(qv_layer_status, key_nexts, 254);
QV_ASSERT_SIZE(qv_hud_header, 4096);
QV_ASSERT_OFFSET(qv_hud_header, magic, 0);
QV_ASSERT_OFFSET(qv_hud_header, version, 8);
QV_ASSERT_OFFSET(qv_hud_header, header_bytes, 10);
QV_ASSERT_OFFSET(qv_hud_header, buffer_bytes, 12);
QV_ASSERT_OFFSET(qv_hud_header, published, 64);
QV_ASSERT_OFFSET(qv_hud_header, heartbeat, 72);
QV_ASSERT_OFFSET(qv_hud_header, writer_pid, 80);
QV_ASSERT_SIZE(qv_hud_graph, 60);
QV_ASSERT_OFFSET(qv_hud_graph, x, 0);
QV_ASSERT_OFFSET(qv_hud_graph, y, 4);
QV_ASSERT_OFFSET(qv_hud_graph, w, 8);
QV_ASSERT_OFFSET(qv_hud_graph, h, 12);
QV_ASSERT_OFFSET(qv_hud_graph, span_ms, 16);
QV_ASSERT_OFFSET(qv_hud_graph, min_ms, 20);
QV_ASSERT_OFFSET(qv_hud_graph, max_ms, 24);
QV_ASSERT_OFFSET(qv_hud_graph, line, 28);
QV_ASSERT_OFFSET(qv_hud_graph, fill, 32);
QV_ASSERT_OFFSET(qv_hud_graph, muted, 36);
QV_ASSERT_OFFSET(qv_hud_graph, warn_above_ms, 40);
QV_ASSERT_OFFSET(qv_hud_graph, bad_above_ms, 44);
QV_ASSERT_OFFSET(qv_hud_graph, warn, 48);
QV_ASSERT_OFFSET(qv_hud_graph, bad, 52);
QV_ASSERT_OFFSET(qv_hud_graph, series, 56);
QV_ASSERT_SIZE(qv_hud_buffer, 256);
QV_ASSERT_OFFSET(qv_hud_buffer, seq, 0);
QV_ASSERT_OFFSET(qv_hud_buffer, width, 8);
QV_ASSERT_OFFSET(qv_hud_buffer, height, 12);
QV_ASSERT_OFFSET(qv_hud_buffer, stride, 16);
QV_ASSERT_OFFSET(qv_hud_buffer, format, 20);
QV_ASSERT_OFFSET(qv_hud_buffer, scale, 24);
QV_ASSERT_OFFSET(qv_hud_buffer, extent_w, 28);
QV_ASSERT_OFFSET(qv_hud_buffer, extent_h, 32);
QV_ASSERT_OFFSET(qv_hud_buffer, x, 36);
QV_ASSERT_OFFSET(qv_hud_buffer, y, 40);
QV_ASSERT_OFFSET(qv_hud_buffer, opacity, 44);
QV_ASSERT_OFFSET(qv_hud_buffer, ngraphs, 48);
QV_ASSERT_OFFSET(qv_hud_buffer, graph, 52);
QV_ASSERT_OFFSET(qv_hud_buffer, model_rev, 176);

/* ---- Ring writer ------------------------------------------------------------------- *
 * For a ring this process writes (a mod's ring the bridge created; map it with
 * CreateFileW + CreateFileMappingW + MapViewOfFile on the Z:\ path). Lock-free and
 * syscall-free; any thread may call qv_ring_push. Protocol (same as quiver-perf's RingWriter):
 *   i = fetch_add(reserve, 1); slot = i & (slot_count - 1)
 *   claim: CAS commit from c (not busy, c < i+1) to (i+1)|BUSY, else drop and count
 *   release fence; store words 1..15; store commit = i+1 (release)
 * Readers detect overruns and torn copies from commit; see quiver-perf/src/ring.rs. */

#if defined(_MSC_VER) && !defined(__clang__)
#include <intrin.h>
#if defined(_M_IX86)
/* 32-bit: a plain 64-bit access is two 32-bit ones; go through the interlocked ops. */
QV_INLINE uint64_t qv__load_relaxed(const volatile uint64_t *p)
{ return (uint64_t)_InterlockedCompareExchange64((volatile __int64 *)p, 0, 0); }
QV_INLINE void qv__store_relaxed(volatile uint64_t *p, uint64_t v) { _InterlockedExchange64((volatile __int64 *)p, (__int64)v); }
QV_INLINE void qv__store_release(volatile uint64_t *p, uint64_t v) { _InterlockedExchange64((volatile __int64 *)p, (__int64)v); }
#else
QV_INLINE uint64_t qv__load_relaxed(const volatile uint64_t *p) { return *p; }
QV_INLINE void qv__store_relaxed(volatile uint64_t *p, uint64_t v) { *p = v; }
QV_INLINE void qv__store_release(volatile uint64_t *p, uint64_t v) { _ReadWriteBarrier(); *p = v; }
#endif
QV_INLINE void qv__fence_release(void) { _ReadWriteBarrier(); }
QV_INLINE uint64_t qv__fetch_add(volatile uint64_t *p, uint64_t v)
{ return (uint64_t)_InterlockedExchangeAdd64((volatile __int64 *)p, (__int64)v); }
QV_INLINE int qv__cas(volatile uint64_t *p, uint64_t *expected, uint64_t desired)
{
    __int64 old = _InterlockedCompareExchange64((volatile __int64 *)p, (__int64)desired, (__int64)*expected);
    if ((uint64_t)old == *expected) return 1;
    *expected = (uint64_t)old;
    return 0;
}
#else
QV_INLINE uint64_t qv__load_relaxed(const volatile uint64_t *p) { return __atomic_load_n(p, __ATOMIC_RELAXED); }
QV_INLINE void qv__store_relaxed(volatile uint64_t *p, uint64_t v) { __atomic_store_n(p, v, __ATOMIC_RELAXED); }
QV_INLINE void qv__store_release(volatile uint64_t *p, uint64_t v) { __atomic_store_n(p, v, __ATOMIC_RELEASE); }
QV_INLINE void qv__fence_release(void) { __atomic_thread_fence(__ATOMIC_RELEASE); }
QV_INLINE uint64_t qv__fetch_add(volatile uint64_t *p, uint64_t v) { return __atomic_fetch_add(p, v, __ATOMIC_RELAXED); }
QV_INLINE int qv__cas(volatile uint64_t *p, uint64_t *expected, uint64_t desired)
{ return __atomic_compare_exchange_n(p, expected, desired, 0, __ATOMIC_RELAXED, __ATOMIC_RELAXED); }
#endif

/* Header checks a writer should make once after mapping (the file came from someone else).
 * Returns 1 if usable. `mapped_bytes` is the view size. */
QV_INLINE int qv_ring_valid(const qv_ring_header *h, size_t mapped_bytes)
{
    uint32_t n;
    if (mapped_bytes < QV_RING_HEADER_BYTES) return 0;
    if (qv__load_relaxed(&h->magic) != QV_RING_MAGIC) return 0;
    if (h->version != QV_RING_VERSION || h->header_bytes != QV_RING_HEADER_BYTES || h->slot_bytes != QV_SLOT_BYTES) return 0;
    n = h->slot_count;
    if (n < QV_RING_MIN_SLOTS || n > QV_RING_MAX_SLOTS || (n & (n - 1)) != 0) return 0;
    return mapped_bytes >= (size_t)QV_RING_HEADER_BYTES + (size_t)n * QV_SLOT_BYTES;
}

/* Appends `r` (its commit is ignored). Returns 1, or 0 if the slot was busy and the record
 * was dropped (counted in h->dropped). `slot_count` is the validated h->slot_count, cached. */
QV_INLINE int qv_ring_push(qv_ring_header *h, uint32_t slot_count, const qv_record *r)
{
    uint64_t w[QV_SLOT_BYTES / 8];
    uint64_t i = qv__fetch_add(&h->reserve, 1);
    uint64_t want = (i + 1) & ~QV_COMMIT_BUSY;
    volatile uint64_t *slot = (volatile uint64_t *)(qv_ring_slots(h) + (i & (uint64_t)(slot_count - 1)));
    uint64_t c = qv__load_relaxed(slot);
    int k;
    for (;;) {
        if ((c & QV_COMMIT_BUSY) != 0 || c >= want) {
            qv__fetch_add(&h->dropped, 1);
            return 0;
        }
        if (qv__cas(slot, &c, want | QV_COMMIT_BUSY)) break;
    }
    qv__fence_release();
    memcpy(w, r, sizeof w);
    for (k = 1; k < QV_SLOT_BYTES / 8; k++) qv__store_relaxed(slot + k, w[k]);
    qv__store_release(slot, want);
    return 1;
}

QV_INLINE void qv_ring_heartbeat(qv_ring_header *h, int64_t now)
{ qv__store_release(&h->heartbeat, (uint64_t)now); }

QV_INLINE void qv_ring_add_dropped(qv_ring_header *h, uint64_t n)
{ qv__fetch_add(&h->dropped, n); }

/* A series record: f[0] holds the double's bits. */
QV_INLINE void qv_record_series(qv_record *r, uint16_t metric, int64_t t, double value)
{
    memset(r, 0, sizeof *r);
    r->metric = metric;
    r->t = t;
    r->nfields = 1;
    memcpy(&r->f[0], &value, sizeof value);
}

/* An event record: optional duration in f[0], optional label (<= 48 bytes, cut) in f[1..6]. */
QV_INLINE void qv_record_event(qv_record *r, uint16_t metric, int64_t t, const int64_t *duration, const char *label)
{
    memset(r, 0, sizeof *r);
    r->metric = metric;
    r->t = t;
    r->nfields = 1;
    if (duration) { r->f[0] = *duration; r->flags |= QV_FLAG_HAS_DURATION; }
    if (label) {
        size_t n = strlen(label);
        if (n > QV_EVENT_LABEL_BYTES) {
            n = QV_EVENT_LABEL_BYTES;
            while (n > 0 && ((unsigned char)label[n] & 0xC0) == 0x80) n--; /* char boundary */
        }
        memcpy(&r->f[1], label, n);
        r->flags |= QV_FLAG_HAS_LABEL;
        r->nfields = 7;
    }
}

#ifdef __cplusplus
}
#endif
#endif /* QUIVER_PERF_H */
