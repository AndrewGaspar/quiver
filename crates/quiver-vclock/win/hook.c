/* The hook DLL of `quiver lab`'s fixed-step virtual clock (design/LAB.md, "The virtual clock").
 *
 * The game is told that every frame took exactly 1/rate seconds, however long it really took.
 * Two things follow: a game whose frames are cheap runs its simulation faster than real time
 * (600 fps at a 60 Hz virtual rate is 10x), and a game on a busy machine behaves exactly as it
 * would at the virtual rate, only slower than real time: the same frames see the same time,
 * so runs repeat.
 *
 * Built as a proxy for a system DLL the game imports (version, winmm, dinput8 or dsound; the
 * generated stubs forward every export to the real one in the system folder) and placed by the
 * lab in the instance's install clone, next to the game's executable, with the DLL overridden
 * native-then-builtin for that launch. Without QUIVER_VCLOCK in the environment it is a pure
 * pass-through.
 *
 * With it (the rate in Hz), the clock is vclock.c's and presentation is uncapped.
 *
 * Where: the import address tables of the GAME's own modules only (the executable, plus the
 * modules named in QUIVER_VCLOCK_MODULES), matched by imported name at run time. Never the
 * kernel32/winmm exports: Wine and DXVK read the same functions for their own pacing, and this
 * DLL reads the real clock through its own imports, which is what makes its real-time numbers
 * honest. Imports are QueryPerformanceCounter/Frequency, GetTickCount(64), timeGetTime,
 * GetSystemTimeAsFileTime (and the Precise variant), Sleep and SleepEx, from kernel32,
 * kernelbase, the api-ms-win-core sets and winmm; GetProcAddress is hooked in the same modules
 * so names looked up at run time get the same answer.
 *
 * The frame boundary: the game's own Present, hooked in process. Creation functions (Direct3D 8
 * and 9, DXGI factories, D3D10/11 device-and-swap-chain, OpenGL SwapBuffers, Vulkan
 * vkQueuePresentKHR and its proc-address lookups) are hooked in the imports of the game's
 * modules and of the other modules in its folder; the objects they return get their Present
 * vtable slots hooked. A Present advances the clock on the game's thread, right when the game
 * finishes a frame (a Vulkan layer sees DXVK's present a frame later on DXVK's own thread).
 *
 * Reports: QUIVER_VCLOCK_DIR (else QUIVER_LAB_RUN_DIR) gets vclock.log (what was
 * hooked, engagement, a report every 10 s) and vclock.status (key=value, rewritten every
 * half second; `quiver lab status` reads it).
 *
 * This changes time only. It never touches licence, ownership or DRM code.
 *
 * Prior art: the virtual clock of the ds-decomp project's RIG shim (docs/17-virtual-clock.md). */
#define WIN32_LEAN_AND_MEAN
#define __USE_MINGW_ANSI_STDIO 1
#define PSAPI_VERSION 2
#define COBJMACROS
#define CINTERFACE
#include <windows.h>
#include <psapi.h>
#include <initguid.h>
#include <d3d9.h>
#include <dxgi1_2.h>
#include <d3d11.h>

#include <stddef.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vclock.h"

/* The proxy part (generated per DLL name and architecture by build.rs). */
extern const char ss_proxy[];
extern const int ss_count;
extern const char *const ss_names[];
extern const unsigned short ss_ords[];
extern void *ss_real[];

/* Marks this DLL so the lab recognises (and removes) copies it left behind. */
__attribute__((used)) const char ss_marker[] = "QUIVER-VCLOCK-DLL v1";

#define SLOT(iface, method) ((unsigned)(offsetof(iface##Vtbl, method) / sizeof(void *)))

/* ------------------------------------------------------------------------------------------ */
/* State */

static HMODULE g_self, g_real, g_exe;
static int g_on;
static vc_clock g_clock;

static struct {
    double rate;
    uint32_t creep_after, stuck_ms, sleep_max_ms;
    int sleeps; /* which sleeps get shorter: 0 none, 1 the presenting thread's, 2 all */
    char exe[MAX_PATH];
    int sync0;
    char modules[1024]; /* lowercase names, ';'-separated */
    char dir[MAX_PATH];
} g_cfg;

static WCHAR g_exe_dir[MAX_PATH]; /* lowercase, with the trailing backslash */
static BOOL (WINAPI *p_real_qpc)(LARGE_INTEGER *);
static BOOL (WINAPI *p_real_qpf)(LARGE_INTEGER *);
static ULONGLONG (WINAPI *p_real_gtc64)(void);

/* Clocks' origins: the real values at install, so virtual values start where real ones were. */
static uint64_t g_tick_origin, g_tick64_origin, g_mm_origin, g_ft_origin;
static int g_mm_origin_set;

static CRITICAL_SECTION g_cs;      /* log, status, module list, vtable registry writes */
static HANDLE g_log = INVALID_HANDLE_VALUE;
static DWORD g_tls = TLS_OUT_OF_INDEXES;     /* present nesting depth */
static DWORD g_tls_thr = TLS_OUT_OF_INDEXES; /* the thread's vc_thread */
static vc_thread g_thr_fallback;             /* if a thread's couldn't be allocated */
static volatile DWORD g_present_tid;         /* the thread that last presented */
static int g_main = 1;                       /* this process is the game (QUIVER_VCLOCK_EXE) */
static volatile LONG g_quit;

enum { PATH_NONE, PATH_VK, PATH_GL, PATH_D3D8, PATH_D3D9, PATH_DXGI };
static const char *const PATH_NAMES[] = {"none", "vulkan", "opengl", "d3d8", "d3d9", "dxgi"};
static volatile LONG g_path;
static volatile LONG g_ignored_presents;
static volatile LONG g_patched_slots, g_vtable_hooks;
static char g_hooked[512]; /* what got hooked, for the status ("dxgi.Present d3d9.Present …") */

/* ASCII lowercase in place (paths are compared after the same folding on both sides; no user32). */
static void lower_w(WCHAR *s, DWORD n) {
    for (DWORD i = 0; i < n && s[i]; i++)
        if (s[i] >= L'A' && s[i] <= L'Z') s[i] = (WCHAR)(s[i] + 32);
}

/* The delta census: the distinct steps the hooked clocks returned to the game, between one
 * value and the next larger one (performance-counter ticks; and milliseconds, shared by
 * timeGetTime and GetTickCount(64)). On the virtual clock it is one value, a frame; creep shows
 * as small steps and an escape as odd ones. Counted from engagement. */
#define NBINS 12
typedef struct {
    int64_t last;
    struct { int64_t delta; uint64_t count; } b[NBINS];
    uint64_t other, steps;
} census;
static census g_cqpc = {INT64_MIN, {{0, 0}}, 0, 0}, g_cms = {INT64_MIN, {{0, 0}}, 0, 0};
static CRITICAL_SECTION g_census_cs;

static void census_add(census *c, int64_t v) {
    if (!__atomic_load_n(&g_clock.engaged, __ATOMIC_ACQUIRE)) return;
    int64_t cur = __atomic_load_n(&c->last, __ATOMIC_ACQUIRE);
    for (;;) {
        if (v <= cur && cur != INT64_MIN) return;
        if (__atomic_compare_exchange_n(&c->last, &cur, v, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
    }
    if (cur == INT64_MIN) return; /* the baseline */
    int64_t d = v - cur;
    EnterCriticalSection(&g_census_cs);
    c->steps++;
    int i = 0;
    for (; i < NBINS && c->b[i].count; i++)
        if (c->b[i].delta == d) break;
    if (i == NBINS) c->other++;
    else {
        c->b[i].delta = d;
        c->b[i].count++;
    }
    LeaveCriticalSection(&g_census_cs);
}

static void census_text(census *c, char *out, size_t n) {
    size_t used = 0;
    out[0] = 0;
    EnterCriticalSection(&g_census_cs);
    for (int i = 0; i < NBINS && c->b[i].count && used + 48 < n; i++)
        used += (size_t)snprintf(out + used, n - used, "%s%lld:%llu", i ? "," : "", (long long)c->b[i].delta, (unsigned long long)c->b[i].count);
    if (!used) snprintf(out, n, "-");
    LeaveCriticalSection(&g_census_cs);
}

static int64_t real_now(void *ctx) {
    (void)ctx;
    LARGE_INTEGER t;
    p_real_qpc(&t);
    return t.QuadPart;
}

static double real_since_install(void) {
    return (double)(real_now(NULL) - g_clock.real_origin) / (double)g_clock.real_freq;
}

/* ------------------------------------------------------------------------------------------ */
/* Log and status */

static void ss_log(const char *fmt, ...) {
    if (g_log == INVALID_HANDLE_VALUE) return;
    char buf[1024];
    int n = snprintf(buf, sizeof buf, "[vclock pid %lu %9.3f] ", (unsigned long)GetCurrentProcessId(), g_clock.real_freq ? real_since_install() : 0.0);
    va_list ap;
    va_start(ap, fmt);
    int m = vsnprintf(buf + n, sizeof buf - (size_t)n - 2, fmt, ap);
    va_end(ap);
    if (m < 0) m = 0;
    n += m;
    if (n > (int)sizeof buf - 2) n = (int)sizeof buf - 2;
    buf[n++] = '\n';
    DWORD w;
    EnterCriticalSection(&g_cs);
    WriteFile(g_log, buf, (DWORD)n, &w, NULL);
    LeaveCriticalSection(&g_cs);
}

static void note_hooked(const char *what) {
    EnterCriticalSection(&g_cs);
    if (!strstr(g_hooked, what) && strlen(g_hooked) + strlen(what) + 2 < sizeof g_hooked) {
        if (g_hooked[0]) strcat(g_hooked, " ");
        strcat(g_hooked, what);
    }
    LeaveCriticalSection(&g_cs);
}

static void write_status(const char *why) {
    if (!g_cfg.dir[0]) return;
    vc_clock *c = &g_clock;
    /* A helper process (a crash handler, a launcher) loads the proxy too: it never replaces
     * the game's report unless it is the one presenting. */
    if (!g_main && !__atomic_load_n(&c->engaged, __ATOMIC_ACQUIRE)) return;
    double vs = vc_virtual_seconds(c), rs = vc_real_seconds(c);
    char cq[512], cm[512];
    census_text(&g_cqpc, cq, sizeof cq);
    census_text(&g_cms, cm, sizeof cm);
    char buf[6144];
    EnterCriticalSection(&g_cs);
    int n = snprintf(buf, sizeof buf,
        "version=1\nwhy=%s\npid=%lu\nproxy=%s.dll\narch=%s\nrate=%.6f\nticks_per_frame=%lld\nqpf=%lld\nreal_qpf=%lld\n"
        "engaged=%d\npresent_path=%s\nignored_presents=%ld\nhooked=%s\npatched_slots=%ld\nvtable_hooks=%ld\nmodules=%s\n"
        "frames=%llu\nvirtual_s=%.6f\nreal_s=%.6f\nspeedup=%.4f\nqueries=%llu\nworst_frame_queries=%llu\n"
        "creep_after=%llu\ncreep_frames=%llu\nstuck_ms=%u\nescapes=%llu\noverruns=%llu\n"
        "sleeps=%llu\nsleep_ms_requested=%llu\nsleep_ms_slept=%llu\nsleep_max_ms=%u\nsync0=%d\nsince_install_s=%.3f\n"
        "census_qpc=%s\ncensus_qpc_other=%llu\ncensus_ms=%s\ncensus_ms_other=%llu\n"
        "sleeps_real=%llu\nsleep_mode=%s\npresent_thread=%lu\ngame_process=%d\n",
        why, (unsigned long)GetCurrentProcessId(), ss_proxy, sizeof(void *) == 8 ? "x64" : "x86", c->rate,
        (long long)c->ticks_per_frame, (long long)c->vfreq, (long long)c->real_freq, (int)__atomic_load_n(&c->engaged, __ATOMIC_ACQUIRE),
        PATH_NAMES[g_path], (long)g_ignored_presents, g_hooked[0] ? g_hooked : "-", (long)g_patched_slots, (long)g_vtable_hooks,
        g_cfg.modules[0] ? g_cfg.modules : "-",
        (unsigned long long)vc_load_u64(&c->frames), vs, rs, rs > 0.0 ? vs / rs : 0.0,
        (unsigned long long)vc_load_u64(&c->queries_total), (unsigned long long)vc_load_u64(&c->queries_max),
        (unsigned long long)c->creep_after, (unsigned long long)vc_load_u64(&c->creep_frames), c->stuck_ms,
        (unsigned long long)vc_load_u64(&c->escapes), (unsigned long long)vc_load_u64(&c->overruns),
        (unsigned long long)vc_load_u64(&c->sleeps), (unsigned long long)vc_load_u64(&c->sleep_ms_requested),
        (unsigned long long)vc_load_u64(&c->sleep_ms_slept), g_cfg.sleep_max_ms, g_cfg.sync0, real_since_install(),
        cq, (unsigned long long)g_cqpc.other, cm, (unsigned long long)g_cms.other,
        (unsigned long long)vc_load_u64(&c->sleeps_real), g_cfg.sleeps == 2 ? "all" : g_cfg.sleeps == 1 ? "present" : "none",
        (unsigned long)g_present_tid, g_main);
    LeaveCriticalSection(&g_cs);
    if (n <= 0) return;
    if (n > (int)sizeof buf) n = (int)sizeof buf;
    char path[MAX_PATH + 32], tmp[MAX_PATH + 48];
    snprintf(path, sizeof path, "%s\\vclock.status", g_cfg.dir);
    snprintf(tmp, sizeof tmp, "%s\\vclock.status.%lu.tmp", g_cfg.dir, (unsigned long)GetCurrentProcessId());
    HANDLE h = CreateFileA(tmp, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) return;
    DWORD w;
    WriteFile(h, buf, (DWORD)n, &w, NULL);
    CloseHandle(h);
    MoveFileExA(tmp, path, MOVEFILE_REPLACE_EXISTING);
}

static void report(const char *tag) {
    vc_clock *c = &g_clock;
    double vs = vc_virtual_seconds(c), rs = vc_real_seconds(c);
    ss_log("%s: %s  frames=%llu  virtual=%.3fs  real=%.3fs  speedup=%.3fx  presents from %s (ignored %ld)  "
           "reads=%llu worst thread-frame=%llu  creeping frames=%llu  escapes=%llu  overruns=%llu  sleeps shortened=%llu (%llu ms asked, %llu slept)  kept real=%llu",
           tag, c->engaged ? "engaged" : "PASS-THROUGH (no Present yet)", (unsigned long long)vc_load_u64(&c->frames), vs, rs,
           rs > 0 ? vs / rs : 0.0, PATH_NAMES[g_path], (long)g_ignored_presents, (unsigned long long)vc_load_u64(&c->queries_total),
           (unsigned long long)vc_load_u64(&c->queries_max), (unsigned long long)vc_load_u64(&c->creep_frames),
           (unsigned long long)vc_load_u64(&c->escapes), (unsigned long long)vc_load_u64(&c->overruns),
           (unsigned long long)vc_load_u64(&c->sleeps), (unsigned long long)vc_load_u64(&c->sleep_ms_requested),
           (unsigned long long)vc_load_u64(&c->sleep_ms_slept), (unsigned long long)vc_load_u64(&c->sleeps_real));
}

/* ------------------------------------------------------------------------------------------ */
/* Frame boundaries */

static int path_rank(int p) {
    return p == PATH_DXGI || p == PATH_D3D9 || p == PATH_D3D8 ? 3 : p == PATH_GL ? 2 : p == PATH_VK ? 1 : 0;
}

/* One present path drives the clock: the first seen, unless a higher-level one shows up (a
 * D3D Present outranks the Vulkan present a translation layer makes for it on another
 * thread). A frame presented twice through two paths would otherwise count twice. */
static void on_present(int path) {
    if (!g_on) return;
    LONG cur = g_path;
    if (cur != path) {
        if (cur != PATH_NONE && path_rank(path) <= path_rank(cur)) {
            InterlockedIncrement(&g_ignored_presents);
            return;
        }
        if (InterlockedCompareExchange(&g_path, path, cur) != cur) return;
        if (cur) ss_log("presents are now counted from %s (was %s: a higher-level present path showed up)", PATH_NAMES[path], PATH_NAMES[cur]);
        else ss_log("presents are counted from %s", PATH_NAMES[path]);
    }
    int was = __atomic_load_n(&g_clock.engaged, __ATOMIC_ACQUIRE);
    vc_present(&g_clock);
    g_present_tid = GetCurrentThreadId();
    if (!was) {
        ss_log("engaged at the first %s Present: virtual time starts at %.3f s; every frame is now %lld/%lld s (%.9f s)",
               PATH_NAMES[path], (double)g_clock.vnow / (double)g_clock.vfreq, (long long)g_clock.ticks_per_frame,
               (long long)g_clock.vfreq, (double)g_clock.ticks_per_frame / (double)g_clock.vfreq);
        write_status("engaged");
    }
}

/* Nested presents on one thread (a device Present calling its swap chain's) count once. */
static int guard_enter(void) {
    DWORD err = GetLastError();
    uintptr_t d = (uintptr_t)TlsGetValue(g_tls);
    TlsSetValue(g_tls, (void *)(d + 1));
    SetLastError(err);
    return d == 0;
}

static void guard_leave(void) {
    DWORD err = GetLastError();
    uintptr_t d = (uintptr_t)TlsGetValue(g_tls);
    TlsSetValue(g_tls, (void *)(d ? d - 1 : 0));
    SetLastError(err);
}

/* ------------------------------------------------------------------------------------------ */
/* Vtable hooks: a registry of (vtable, slot) -> original, so one hook serves every class. */

typedef struct { void **vt; unsigned slot; void *orig; } vhook;
#define MAX_VH 128
static vhook g_vh[MAX_VH];
static volatile LONG g_nvh;

static void *vt_orig(void *obj, unsigned slot) {
    void **vt = *(void ***)obj;
    LONG n = __atomic_load_n(&g_nvh, __ATOMIC_ACQUIRE);
    for (LONG i = 0; i < n; i++)
        if (g_vh[i].vt == vt && g_vh[i].slot == slot) return g_vh[i].orig;
    return NULL;
}

static void vt_hook(void *obj, unsigned slot, void *hook, const char *what) {
    if (!obj) return;
    EnterCriticalSection(&g_cs);
    void **vt = *(void ***)obj;
    if (vt[slot] == hook) {
        LeaveCriticalSection(&g_cs);
        return;
    }
    LONG n = g_nvh;
    if (n >= MAX_VH) {
        LeaveCriticalSection(&g_cs);
        ss_log("too many vtable hooks; %s (vtable %p) left alone", what, (void *)vt);
        return;
    }
    g_vh[n].vt = vt;
    g_vh[n].slot = slot;
    g_vh[n].orig = vt[slot];
    __atomic_store_n(&g_nvh, n + 1, __ATOMIC_RELEASE);
    DWORD old;
    if (VirtualProtect(&vt[slot], sizeof(void *), PAGE_EXECUTE_READWRITE, &old)) {
        __atomic_store_n(&vt[slot], hook, __ATOMIC_RELEASE);
        VirtualProtect(&vt[slot], sizeof(void *), old, &old);
        InterlockedIncrement(&g_vtable_hooks);
        LeaveCriticalSection(&g_cs);
        ss_log("hooked %s (vtable %p slot %u)", what, (void *)vt, slot);
        note_hooked(what);
    } else {
        LeaveCriticalSection(&g_cs);
        ss_log("couldn't unprotect %s's vtable %p", what, (void *)vt);
    }
}

/* DXGI ------------------------------------------------------------------------------------- */

typedef HRESULT(STDMETHODCALLTYPE *pfn_present)(IDXGISwapChain *, UINT, UINT);
typedef HRESULT(STDMETHODCALLTYPE *pfn_present1)(IDXGISwapChain1 *, UINT, UINT, const DXGI_PRESENT_PARAMETERS *);

static HRESULT STDMETHODCALLTYPE h_sc_Present(IDXGISwapChain *sc, UINT sync, UINT flags) {
    pfn_present o = (pfn_present)vt_orig(sc, SLOT(IDXGISwapChain, Present));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    if (g_on && g_cfg.sync0 && !(flags & DXGI_PRESENT_TEST)) sync = 0;
    int outer = guard_enter();
    HRESULT hr = o(sc, sync, flags);
    if (outer && SUCCEEDED(hr) && !(flags & DXGI_PRESENT_TEST)) on_present(PATH_DXGI);
    guard_leave();
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_sc_Present1(IDXGISwapChain1 *sc, UINT sync, UINT flags, const DXGI_PRESENT_PARAMETERS *p) {
    pfn_present1 o = (pfn_present1)vt_orig(sc, SLOT(IDXGISwapChain1, Present1));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    if (g_on && g_cfg.sync0 && !(flags & DXGI_PRESENT_TEST)) sync = 0;
    int outer = guard_enter();
    HRESULT hr = o(sc, sync, flags, p);
    if (outer && SUCCEEDED(hr) && !(flags & DXGI_PRESENT_TEST)) on_present(PATH_DXGI);
    guard_leave();
    return hr;
}

static void hook_swapchain(IUnknown *u) {
    if (!u) return;
    IDXGISwapChain *sc = NULL;
    if (SUCCEEDED(IUnknown_QueryInterface(u, &IID_IDXGISwapChain, (void **)&sc)) && sc) {
        vt_hook(sc, SLOT(IDXGISwapChain, Present), (void *)h_sc_Present, "dxgi.Present");
        IDXGISwapChain_Release(sc);
    }
    IDXGISwapChain1 *sc1 = NULL;
    if (SUCCEEDED(IUnknown_QueryInterface(u, &IID_IDXGISwapChain1, (void **)&sc1)) && sc1) {
        vt_hook(sc1, SLOT(IDXGISwapChain1, Present), (void *)h_sc_Present, "dxgi.Present");
        vt_hook(sc1, SLOT(IDXGISwapChain1, Present1), (void *)h_sc_Present1, "dxgi.Present1");
        IDXGISwapChain1_Release(sc1);
    }
}

typedef HRESULT(STDMETHODCALLTYPE *pfn_create_sc)(IDXGIFactory *, IUnknown *, DXGI_SWAP_CHAIN_DESC *, IDXGISwapChain **);
typedef HRESULT(STDMETHODCALLTYPE *pfn_create_sc_hwnd)(IDXGIFactory2 *, IUnknown *, HWND, const DXGI_SWAP_CHAIN_DESC1 *,
                                                       const DXGI_SWAP_CHAIN_FULLSCREEN_DESC *, IDXGIOutput *, IDXGISwapChain1 **);
typedef HRESULT(STDMETHODCALLTYPE *pfn_create_sc_cw)(IDXGIFactory2 *, IUnknown *, IUnknown *, const DXGI_SWAP_CHAIN_DESC1 *,
                                                     IDXGIOutput *, IDXGISwapChain1 **);
typedef HRESULT(STDMETHODCALLTYPE *pfn_create_sc_comp)(IDXGIFactory2 *, IUnknown *, const DXGI_SWAP_CHAIN_DESC1 *, IDXGIOutput *,
                                                       IDXGISwapChain1 **);

static HRESULT STDMETHODCALLTYPE h_f_CreateSwapChain(IDXGIFactory *f, IUnknown *dev, DXGI_SWAP_CHAIN_DESC *d, IDXGISwapChain **out) {
    pfn_create_sc o = (pfn_create_sc)vt_orig(f, SLOT(IDXGIFactory, CreateSwapChain));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    HRESULT hr = o(f, dev, d, out);
    if (SUCCEEDED(hr) && out && *out) hook_swapchain((IUnknown *)*out);
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_f_CreateSwapChainForHwnd(IDXGIFactory2 *f, IUnknown *dev, HWND w, const DXGI_SWAP_CHAIN_DESC1 *d,
                                                            const DXGI_SWAP_CHAIN_FULLSCREEN_DESC *fs, IDXGIOutput *o2, IDXGISwapChain1 **out) {
    pfn_create_sc_hwnd o = (pfn_create_sc_hwnd)vt_orig(f, SLOT(IDXGIFactory2, CreateSwapChainForHwnd));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    HRESULT hr = o(f, dev, w, d, fs, o2, out);
    if (SUCCEEDED(hr) && out && *out) hook_swapchain((IUnknown *)*out);
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_f_CreateSwapChainForCoreWindow(IDXGIFactory2 *f, IUnknown *dev, IUnknown *w, const DXGI_SWAP_CHAIN_DESC1 *d,
                                                                 IDXGIOutput *o2, IDXGISwapChain1 **out) {
    pfn_create_sc_cw o = (pfn_create_sc_cw)vt_orig(f, SLOT(IDXGIFactory2, CreateSwapChainForCoreWindow));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    HRESULT hr = o(f, dev, w, d, o2, out);
    if (SUCCEEDED(hr) && out && *out) hook_swapchain((IUnknown *)*out);
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_f_CreateSwapChainForComposition(IDXGIFactory2 *f, IUnknown *dev, const DXGI_SWAP_CHAIN_DESC1 *d,
                                                                  IDXGIOutput *o2, IDXGISwapChain1 **out) {
    pfn_create_sc_comp o = (pfn_create_sc_comp)vt_orig(f, SLOT(IDXGIFactory2, CreateSwapChainForComposition));
    if (!o) return DXGI_ERROR_INVALID_CALL;
    HRESULT hr = o(f, dev, d, o2, out);
    if (SUCCEEDED(hr) && out && *out) hook_swapchain((IUnknown *)*out);
    return hr;
}

static void hook_factory(IUnknown *u) {
    if (!u) return;
    IDXGIFactory *f = NULL;
    if (SUCCEEDED(IUnknown_QueryInterface(u, &IID_IDXGIFactory, (void **)&f)) && f) {
        vt_hook(f, SLOT(IDXGIFactory, CreateSwapChain), (void *)h_f_CreateSwapChain, "dxgi.CreateSwapChain");
        IDXGIFactory_Release(f);
    }
    IDXGIFactory2 *f2 = NULL;
    if (SUCCEEDED(IUnknown_QueryInterface(u, &IID_IDXGIFactory2, (void **)&f2)) && f2) {
        vt_hook(f2, SLOT(IDXGIFactory2, CreateSwapChain), (void *)h_f_CreateSwapChain, "dxgi.CreateSwapChain");
        vt_hook(f2, SLOT(IDXGIFactory2, CreateSwapChainForHwnd), (void *)h_f_CreateSwapChainForHwnd, "dxgi.CreateSwapChainForHwnd");
        vt_hook(f2, SLOT(IDXGIFactory2, CreateSwapChainForCoreWindow), (void *)h_f_CreateSwapChainForCoreWindow, "dxgi.CreateSwapChainForCoreWindow");
        vt_hook(f2, SLOT(IDXGIFactory2, CreateSwapChainForComposition), (void *)h_f_CreateSwapChainForComposition, "dxgi.CreateSwapChainForComposition");
        IDXGIFactory2_Release(f2);
    }
}

/* A device made without a factory of the game's own: its factory is the adapter's parent. */
static void hook_factory_of_device(IUnknown *dev) {
    if (!dev) return;
    IDXGIDevice *d = NULL;
    if (FAILED(IUnknown_QueryInterface(dev, &IID_IDXGIDevice, (void **)&d)) || !d) return;
    IDXGIAdapter *a = NULL;
    if (SUCCEEDED(IDXGIDevice_GetAdapter(d, &a)) && a) {
        IUnknown *f = NULL;
        if (SUCCEEDED(IDXGIAdapter_GetParent(a, &IID_IDXGIFactory, (void **)&f)) && f) {
            hook_factory(f);
            IUnknown_Release(f);
        }
        IDXGIAdapter_Release(a);
    }
    IDXGIDevice_Release(d);
}

typedef HRESULT(WINAPI *pfn_cdf)(REFIID, void **);
typedef HRESULT(WINAPI *pfn_cdf2)(UINT, REFIID, void **);
static pfn_cdf o_CreateDXGIFactory, o_CreateDXGIFactory1;
static pfn_cdf2 o_CreateDXGIFactory2;

static HRESULT WINAPI h_CreateDXGIFactory(REFIID r, void **pp) {
    HRESULT hr = o_CreateDXGIFactory(r, pp);
    if (SUCCEEDED(hr) && pp && *pp) hook_factory((IUnknown *)*pp);
    return hr;
}
static HRESULT WINAPI h_CreateDXGIFactory1(REFIID r, void **pp) {
    HRESULT hr = o_CreateDXGIFactory1(r, pp);
    if (SUCCEEDED(hr) && pp && *pp) hook_factory((IUnknown *)*pp);
    return hr;
}
static HRESULT WINAPI h_CreateDXGIFactory2(UINT fl, REFIID r, void **pp) {
    HRESULT hr = o_CreateDXGIFactory2(fl, r, pp);
    if (SUCCEEDED(hr) && pp && *pp) hook_factory((IUnknown *)*pp);
    return hr;
}

typedef HRESULT(WINAPI *pfn_d3d11cdasc)(IDXGIAdapter *, D3D_DRIVER_TYPE, HMODULE, UINT, const D3D_FEATURE_LEVEL *, UINT, UINT,
                                        const DXGI_SWAP_CHAIN_DESC *, IDXGISwapChain **, ID3D11Device **, D3D_FEATURE_LEVEL *,
                                        ID3D11DeviceContext **);
typedef HRESULT(WINAPI *pfn_d3d11cd)(IDXGIAdapter *, D3D_DRIVER_TYPE, HMODULE, UINT, const D3D_FEATURE_LEVEL *, UINT, UINT,
                                     ID3D11Device **, D3D_FEATURE_LEVEL *, ID3D11DeviceContext **);
static pfn_d3d11cdasc o_D3D11CreateDeviceAndSwapChain;
static pfn_d3d11cd o_D3D11CreateDevice;

static HRESULT WINAPI h_D3D11CreateDeviceAndSwapChain(IDXGIAdapter *a, D3D_DRIVER_TYPE t, HMODULE sw, UINT fl, const D3D_FEATURE_LEVEL *fls,
                                                      UINT nfl, UINT sdk, const DXGI_SWAP_CHAIN_DESC *d, IDXGISwapChain **sc,
                                                      ID3D11Device **dev, D3D_FEATURE_LEVEL *got, ID3D11DeviceContext **ctx) {
    HRESULT hr = o_D3D11CreateDeviceAndSwapChain(a, t, sw, fl, fls, nfl, sdk, d, sc, dev, got, ctx);
    if (SUCCEEDED(hr)) {
        if (sc && *sc) hook_swapchain((IUnknown *)*sc);
        if (dev && *dev) hook_factory_of_device((IUnknown *)*dev);
    }
    return hr;
}

static HRESULT WINAPI h_D3D11CreateDevice(IDXGIAdapter *a, D3D_DRIVER_TYPE t, HMODULE sw, UINT fl, const D3D_FEATURE_LEVEL *fls, UINT nfl,
                                          UINT sdk, ID3D11Device **dev, D3D_FEATURE_LEVEL *got, ID3D11DeviceContext **ctx) {
    HRESULT hr = o_D3D11CreateDevice(a, t, sw, fl, fls, nfl, sdk, dev, got, ctx);
    if (SUCCEEDED(hr) && dev && *dev) hook_factory_of_device((IUnknown *)*dev);
    return hr;
}

/* D3D10: every argument is pointer-sized or a 32-bit enum, so plain types do. */
typedef HRESULT(WINAPI *pfn_d3d10cdasc)(void *, UINT, HMODULE, UINT, UINT, void *, IDXGISwapChain **, IUnknown **);
static pfn_d3d10cdasc o_D3D10CreateDeviceAndSwapChain, o_D3D10CreateDeviceAndSwapChain1;

static HRESULT WINAPI h_D3D10CreateDeviceAndSwapChain(void *a, UINT t, HMODULE sw, UINT fl, UINT sdk, void *d, IDXGISwapChain **sc, IUnknown **dev) {
    HRESULT hr = o_D3D10CreateDeviceAndSwapChain(a, t, sw, fl, sdk, d, sc, dev);
    if (SUCCEEDED(hr) && sc && *sc) hook_swapchain((IUnknown *)*sc);
    return hr;
}
static HRESULT WINAPI h_D3D10CreateDeviceAndSwapChain1(void *a, UINT t, HMODULE sw, UINT fl, UINT lvl, void *d, IDXGISwapChain **sc, IUnknown **dev) {
    HRESULT hr = o_D3D10CreateDeviceAndSwapChain1(a, t, sw, fl, lvl, d, sc, dev);
    if (SUCCEEDED(hr) && sc && *sc) hook_swapchain((IUnknown *)*sc);
    return hr;
}

/* Direct3D 9 ------------------------------------------------------------------------------- */

typedef HRESULT(STDMETHODCALLTYPE *pfn_d9_present)(IDirect3DDevice9 *, const RECT *, const RECT *, HWND, const RGNDATA *);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d9_presentex)(IDirect3DDevice9Ex *, const RECT *, const RECT *, HWND, const RGNDATA *, DWORD);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d9sc_present)(IDirect3DSwapChain9 *, const RECT *, const RECT *, HWND, const RGNDATA *, DWORD);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d9_reset)(IDirect3DDevice9 *, D3DPRESENT_PARAMETERS *);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d9_create)(IDirect3D9 *, UINT, D3DDEVTYPE, HWND, DWORD, D3DPRESENT_PARAMETERS *, IDirect3DDevice9 **);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d9_createex)(IDirect3D9Ex *, UINT, D3DDEVTYPE, HWND, DWORD, D3DPRESENT_PARAMETERS *,
                                                    D3DDISPLAYMODEEX *, IDirect3DDevice9Ex **);

static void uncap_d3d9(D3DPRESENT_PARAMETERS *pp) {
    if (g_on && g_cfg.sync0 && pp) pp->PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
}

static HRESULT STDMETHODCALLTYPE h_d9_Present(IDirect3DDevice9 *d, const RECT *s, const RECT *t, HWND w, const RGNDATA *r) {
    pfn_d9_present o = (pfn_d9_present)vt_orig(d, SLOT(IDirect3DDevice9, Present));
    if (!o) return D3DERR_INVALIDCALL;
    int outer = guard_enter();
    HRESULT hr = o(d, s, t, w, r);
    if (outer && SUCCEEDED(hr)) on_present(PATH_D3D9);
    guard_leave();
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_d9_PresentEx(IDirect3DDevice9Ex *d, const RECT *s, const RECT *t, HWND w, const RGNDATA *r, DWORD fl) {
    pfn_d9_presentex o = (pfn_d9_presentex)vt_orig(d, SLOT(IDirect3DDevice9Ex, PresentEx));
    if (!o) return D3DERR_INVALIDCALL;
    int outer = guard_enter();
    HRESULT hr = o(d, s, t, w, r, fl);
    if (outer && hr == S_OK) on_present(PATH_D3D9);
    guard_leave();
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_d9sc_Present(IDirect3DSwapChain9 *sc, const RECT *s, const RECT *t, HWND w, const RGNDATA *r, DWORD fl) {
    pfn_d9sc_present o = (pfn_d9sc_present)vt_orig(sc, SLOT(IDirect3DSwapChain9, Present));
    if (!o) return D3DERR_INVALIDCALL;
    int outer = guard_enter();
    HRESULT hr = o(sc, s, t, w, r, fl);
    if (outer && hr == S_OK) on_present(PATH_D3D9);
    guard_leave();
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_d9_Reset(IDirect3DDevice9 *d, D3DPRESENT_PARAMETERS *pp) {
    pfn_d9_reset o = (pfn_d9_reset)vt_orig(d, SLOT(IDirect3DDevice9, Reset));
    if (!o) return D3DERR_INVALIDCALL;
    uncap_d3d9(pp);
    return o(d, pp);
}

static void hook_d3d9_device(IDirect3DDevice9 *dev) {
    if (!dev) return;
    vt_hook(dev, SLOT(IDirect3DDevice9, Present), (void *)h_d9_Present, "d3d9.Present");
    vt_hook(dev, SLOT(IDirect3DDevice9, Reset), (void *)h_d9_Reset, "d3d9.Reset");
    IDirect3DDevice9Ex *ex = NULL;
    if (SUCCEEDED(IDirect3DDevice9_QueryInterface(dev, &IID_IDirect3DDevice9Ex, (void **)&ex)) && ex) {
        vt_hook(ex, SLOT(IDirect3DDevice9Ex, PresentEx), (void *)h_d9_PresentEx, "d3d9.PresentEx");
        IDirect3DDevice9Ex_Release(ex);
    }
    IDirect3DSwapChain9 *sc = NULL;
    if (SUCCEEDED(IDirect3DDevice9_GetSwapChain(dev, 0, &sc)) && sc) {
        vt_hook(sc, SLOT(IDirect3DSwapChain9, Present), (void *)h_d9sc_Present, "d3d9.SwapChain.Present");
        IDirect3DSwapChain9_Release(sc);
    }
}

static HRESULT STDMETHODCALLTYPE h_d9_CreateDevice(IDirect3D9 *d, UINT a, D3DDEVTYPE t, HWND w, DWORD fl, D3DPRESENT_PARAMETERS *pp,
                                                   IDirect3DDevice9 **out) {
    pfn_d9_create o = (pfn_d9_create)vt_orig(d, SLOT(IDirect3D9, CreateDevice));
    if (!o) return D3DERR_INVALIDCALL;
    uncap_d3d9(pp);
    HRESULT hr = o(d, a, t, w, fl, pp, out);
    if (SUCCEEDED(hr) && out) hook_d3d9_device(*out);
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_d9_CreateDeviceEx(IDirect3D9Ex *d, UINT a, D3DDEVTYPE t, HWND w, DWORD fl, D3DPRESENT_PARAMETERS *pp,
                                                     D3DDISPLAYMODEEX *m, IDirect3DDevice9Ex **out) {
    pfn_d9_createex o = (pfn_d9_createex)vt_orig(d, SLOT(IDirect3D9Ex, CreateDeviceEx));
    if (!o) return D3DERR_INVALIDCALL;
    uncap_d3d9(pp);
    HRESULT hr = o(d, a, t, w, fl, pp, m, out);
    if (SUCCEEDED(hr) && out) hook_d3d9_device((IDirect3DDevice9 *)*out);
    return hr;
}

typedef IDirect3D9 *(WINAPI *pfn_d3dcreate9)(UINT);
typedef HRESULT(WINAPI *pfn_d3dcreate9ex)(UINT, IDirect3D9Ex **);
static pfn_d3dcreate9 o_Direct3DCreate9;
static pfn_d3dcreate9ex o_Direct3DCreate9Ex;

static IDirect3D9 *WINAPI h_Direct3DCreate9(UINT sdk) {
    IDirect3D9 *d = o_Direct3DCreate9(sdk);
    if (d) vt_hook(d, SLOT(IDirect3D9, CreateDevice), (void *)h_d9_CreateDevice, "d3d9.CreateDevice");
    return d;
}

static HRESULT WINAPI h_Direct3DCreate9Ex(UINT sdk, IDirect3D9Ex **pp) {
    HRESULT hr = o_Direct3DCreate9Ex(sdk, pp);
    if (SUCCEEDED(hr) && pp && *pp) {
        vt_hook(*pp, SLOT(IDirect3D9Ex, CreateDevice), (void *)h_d9_CreateDevice, "d3d9.CreateDevice");
        vt_hook(*pp, SLOT(IDirect3D9Ex, CreateDeviceEx), (void *)h_d9_CreateDeviceEx, "d3d9.CreateDeviceEx");
    }
    return hr;
}

/* Direct3D 8 (no headers needed: IDirect3D8::CreateDevice and IDirect3DDevice8::Present are
 * both vtable slot 15, every argument pointer-sized or 32-bit). -------------------------------- */

#define D3D8_CREATEDEVICE 15
#define D3D8_PRESENT 15
typedef HRESULT(STDMETHODCALLTYPE *pfn_d8_present)(void *, const RECT *, const RECT *, HWND, const RGNDATA *);
typedef HRESULT(STDMETHODCALLTYPE *pfn_d8_create)(void *, UINT, UINT, HWND, DWORD, void *, void **);
typedef void *(WINAPI *pfn_d3dcreate8)(UINT);
static pfn_d3dcreate8 o_Direct3DCreate8;

static HRESULT STDMETHODCALLTYPE h_d8_Present(void *d, const RECT *s, const RECT *t, HWND w, const RGNDATA *r) {
    pfn_d8_present o = (pfn_d8_present)vt_orig(d, D3D8_PRESENT);
    if (!o) return D3DERR_INVALIDCALL;
    int outer = guard_enter();
    HRESULT hr = o(d, s, t, w, r);
    if (outer && SUCCEEDED(hr)) on_present(PATH_D3D8);
    guard_leave();
    return hr;
}

static HRESULT STDMETHODCALLTYPE h_d8_CreateDevice(void *d, UINT a, UINT t, HWND w, DWORD fl, void *pp, void **out) {
    pfn_d8_create o = (pfn_d8_create)vt_orig(d, D3D8_CREATEDEVICE);
    if (!o) return D3DERR_INVALIDCALL;
    HRESULT hr = o(d, a, t, w, fl, pp, out);
    if (SUCCEEDED(hr) && out && *out) vt_hook(*out, D3D8_PRESENT, (void *)h_d8_Present, "d3d8.Present");
    return hr;
}

static void *WINAPI h_Direct3DCreate8(UINT sdk) {
    void *d = o_Direct3DCreate8(sdk);
    if (d) vt_hook(d, D3D8_CREATEDEVICE, (void *)h_d8_CreateDevice, "d3d8.CreateDevice");
    return d;
}

/* OpenGL ----------------------------------------------------------------------------------- */

typedef BOOL(WINAPI *pfn_swap)(HDC);
static pfn_swap o_SwapBuffers, o_wglSwapBuffers;

static BOOL WINAPI h_SwapBuffers(HDC dc) {
    int outer = guard_enter();
    BOOL ok = o_SwapBuffers(dc);
    if (outer && ok) on_present(PATH_GL);
    guard_leave();
    return ok;
}
static BOOL WINAPI h_wglSwapBuffers(HDC dc) {
    int outer = guard_enter();
    BOOL ok = o_wglSwapBuffers(dc);
    if (outer && ok) on_present(PATH_GL);
    guard_leave();
    return ok;
}

/* Vulkan (a native Vulkan game; never a translation layer's own present: see on_present) ---- */

#if defined(_WIN64)
#define VKAPI
#else
#define VKAPI __stdcall
#endif
typedef void(VKAPI *pfn_vk_void)(void);
typedef int32_t(VKAPI *pfn_vk_present)(void *, const void *);
typedef pfn_vk_void(VKAPI *pfn_vk_gpa)(void *, const char *);
static pfn_vk_present o_vkQueuePresentKHR;
static pfn_vk_gpa o_vkGetInstanceProcAddr, o_vkGetDeviceProcAddr;

static int32_t VKAPI h_vkQueuePresentKHR(void *q, const void *info) {
    int outer = guard_enter();
    int32_t r = o_vkQueuePresentKHR(q, info);
    if (outer && r >= 0) on_present(PATH_VK);
    guard_leave();
    return r;
}

static pfn_vk_void vk_wrap(const char *name, pfn_vk_void real) {
    if (!real || !name || strcmp(name, "vkQueuePresentKHR")) return real;
    if (!o_vkQueuePresentKHR) {
        o_vkQueuePresentKHR = (pfn_vk_present)real;
        note_hooked("vk.QueuePresentKHR");
    }
    return (pfn_vk_present)real == o_vkQueuePresentKHR ? (pfn_vk_void)h_vkQueuePresentKHR : real;
}
static pfn_vk_void VKAPI h_vkGetInstanceProcAddr(void *i, const char *n) { return vk_wrap(n, o_vkGetInstanceProcAddr(i, n)); }
static pfn_vk_void VKAPI h_vkGetDeviceProcAddr(void *d, const char *n) { return vk_wrap(n, o_vkGetDeviceProcAddr(d, n)); }

/* ------------------------------------------------------------------------------------------ */
/* The clock hooks */

static BOOL(WINAPI *o_QPC)(LARGE_INTEGER *);
static BOOL(WINAPI *o_QPF)(LARGE_INTEGER *);
static DWORD(WINAPI *o_GetTickCount)(void);
static ULONGLONG(WINAPI *o_GetTickCount64)(void);
static DWORD(WINAPI *o_timeGetTime)(void);
static void(WINAPI *o_GetSystemTimeAsFileTime)(FILETIME *);
static void(WINAPI *o_GetSystemTimePreciseAsFileTime)(FILETIME *);
static void(WINAPI *o_Sleep)(DWORD);
static DWORD(WINAPI *o_SleepEx)(DWORD, BOOL);
static FARPROC(WINAPI *o_GetProcAddress)(HMODULE, LPCSTR);

/* This thread's reads (vclock.h): allocated on its first read, kept for its life. */
static vc_thread *thr(void) {
    DWORD err = GetLastError();
    vc_thread *t = (vc_thread *)TlsGetValue(g_tls_thr);
    if (!t) {
        t = (vc_thread *)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof *t);
        if (!t) t = &g_thr_fallback;
        TlsSetValue(g_tls_thr, t);
    }
    SetLastError(err);
    return t;
}

static int shorten_sleep(void) {
    return g_cfg.sleeps == 2 || (g_cfg.sleeps == 1 && GetCurrentThreadId() == g_present_tid);
}

static BOOL WINAPI h_QPC(LARGE_INTEGER *out) {
    if (!out) return FALSE;
    out->QuadPart = vc_now(&g_clock, thr());
    census_add(&g_cqpc, out->QuadPart);
    return TRUE;
}
static BOOL WINAPI h_QPF(LARGE_INTEGER *out) {
    if (!out) return FALSE;
    out->QuadPart = g_clock.vfreq;
    return TRUE;
}
static uint64_t vms(void) {
    uint64_t ms = vc_ticks_ms(&g_clock, vc_now(&g_clock, thr()));
    census_add(&g_cms, (int64_t)ms);
    return ms;
}
static DWORD WINAPI h_GetTickCount(void) { return (DWORD)(g_tick_origin + vms()); }
static ULONGLONG WINAPI h_GetTickCount64(void) { return g_tick64_origin + vms(); }
static DWORD WINAPI h_timeGetTime(void) { return (DWORD)(g_mm_origin + vms()); }
static void vfiletime(FILETIME *ft) {
    uint64_t v = g_ft_origin + vc_ticks_100ns(&g_clock, vc_now(&g_clock, thr()));
    ft->dwLowDateTime = (DWORD)(v & 0xffffffffu);
    ft->dwHighDateTime = (DWORD)(v >> 32);
}
static void WINAPI h_GetSystemTimeAsFileTime(FILETIME *ft) {
    if (ft) vfiletime(ft);
}
static void WINAPI h_GetSystemTimePreciseAsFileTime(FILETIME *ft) {
    if (ft) vfiletime(ft);
}
static void WINAPI h_Sleep(DWORD ms) {
    o_Sleep(ms == INFINITE ? ms : vc_sleep(&g_clock, ms, g_cfg.sleep_max_ms, shorten_sleep()));
}
static DWORD WINAPI h_SleepEx(DWORD ms, BOOL alertable) {
    /* Alertable waits stay alertable: a shorter wait is fine, a swallowed APC is not. */
    return o_SleepEx(ms == INFINITE ? ms : vc_sleep(&g_clock, ms, g_cfg.sleep_max_ms, shorten_sleep()), alertable);
}

/* ------------------------------------------------------------------------------------------ */
/* Import tables */

enum { FAM_KERNEL, FAM_WINMM, FAM_D3D9, FAM_D3D8, FAM_DXGI, FAM_D3D11, FAM_D3D10, FAM_GDI, FAM_GL, FAM_VK, FAM_NONE };

/* Ours: the clock (game modules only), present sources (game modules and the other modules in
 * the game's folder), and GetProcAddress (both, special-cased). */
enum { KIND_CLOCK, KIND_PRESENT, KIND_GPA };

typedef struct {
    const char *name;
    int fam;
    int kind;
    void *detour;
    void **orig;
} target;

static target TARGETS[] = {
    {"QueryPerformanceCounter", FAM_KERNEL, KIND_CLOCK, (void *)h_QPC, (void **)&o_QPC},
    {"QueryPerformanceFrequency", FAM_KERNEL, KIND_CLOCK, (void *)h_QPF, (void **)&o_QPF},
    {"GetTickCount", FAM_KERNEL, KIND_CLOCK, (void *)h_GetTickCount, (void **)&o_GetTickCount},
    {"GetTickCount64", FAM_KERNEL, KIND_CLOCK, (void *)h_GetTickCount64, (void **)&o_GetTickCount64},
    {"GetSystemTimeAsFileTime", FAM_KERNEL, KIND_CLOCK, (void *)h_GetSystemTimeAsFileTime, (void **)&o_GetSystemTimeAsFileTime},
    {"GetSystemTimePreciseAsFileTime", FAM_KERNEL, KIND_CLOCK, (void *)h_GetSystemTimePreciseAsFileTime, (void **)&o_GetSystemTimePreciseAsFileTime},
    {"Sleep", FAM_KERNEL, KIND_CLOCK, (void *)h_Sleep, (void **)&o_Sleep},
    {"SleepEx", FAM_KERNEL, KIND_CLOCK, (void *)h_SleepEx, (void **)&o_SleepEx},
    {"timeGetTime", FAM_WINMM, KIND_CLOCK, (void *)h_timeGetTime, (void **)&o_timeGetTime},
    {"Direct3DCreate9", FAM_D3D9, KIND_PRESENT, (void *)h_Direct3DCreate9, (void **)&o_Direct3DCreate9},
    {"Direct3DCreate9Ex", FAM_D3D9, KIND_PRESENT, (void *)h_Direct3DCreate9Ex, (void **)&o_Direct3DCreate9Ex},
    {"Direct3DCreate8", FAM_D3D8, KIND_PRESENT, (void *)h_Direct3DCreate8, (void **)&o_Direct3DCreate8},
    {"CreateDXGIFactory", FAM_DXGI, KIND_PRESENT, (void *)h_CreateDXGIFactory, (void **)&o_CreateDXGIFactory},
    {"CreateDXGIFactory1", FAM_DXGI, KIND_PRESENT, (void *)h_CreateDXGIFactory1, (void **)&o_CreateDXGIFactory1},
    {"CreateDXGIFactory2", FAM_DXGI, KIND_PRESENT, (void *)h_CreateDXGIFactory2, (void **)&o_CreateDXGIFactory2},
    {"D3D11CreateDeviceAndSwapChain", FAM_D3D11, KIND_PRESENT, (void *)h_D3D11CreateDeviceAndSwapChain, (void **)&o_D3D11CreateDeviceAndSwapChain},
    {"D3D11CreateDevice", FAM_D3D11, KIND_PRESENT, (void *)h_D3D11CreateDevice, (void **)&o_D3D11CreateDevice},
    {"D3D10CreateDeviceAndSwapChain", FAM_D3D10, KIND_PRESENT, (void *)h_D3D10CreateDeviceAndSwapChain, (void **)&o_D3D10CreateDeviceAndSwapChain},
    {"D3D10CreateDeviceAndSwapChain1", FAM_D3D10, KIND_PRESENT, (void *)h_D3D10CreateDeviceAndSwapChain1, (void **)&o_D3D10CreateDeviceAndSwapChain1},
    {"SwapBuffers", FAM_GDI, KIND_PRESENT, (void *)h_SwapBuffers, (void **)&o_SwapBuffers},
    {"wglSwapBuffers", FAM_GL, KIND_PRESENT, (void *)h_wglSwapBuffers, (void **)&o_wglSwapBuffers},
    {"vkQueuePresentKHR", FAM_VK, KIND_PRESENT, (void *)h_vkQueuePresentKHR, (void **)&o_vkQueuePresentKHR},
    {"vkGetInstanceProcAddr", FAM_VK, KIND_PRESENT, (void *)h_vkGetInstanceProcAddr, (void **)&o_vkGetInstanceProcAddr},
    {"vkGetDeviceProcAddr", FAM_VK, KIND_PRESENT, (void *)h_vkGetDeviceProcAddr, (void **)&o_vkGetDeviceProcAddr},
    {"GetProcAddress", FAM_KERNEL, KIND_GPA, NULL, (void **)&o_GetProcAddress},
};
#define NTARGETS ((int)(sizeof TARGETS / sizeof TARGETS[0]))

static int lower_eq(const char *a, const char *b) {
    for (; *a && *b; a++, b++) {
        char x = (*a >= 'A' && *a <= 'Z') ? (char)(*a + 32) : *a;
        char y = (*b >= 'A' && *b <= 'Z') ? (char)(*b + 32) : *b;
        if (x != y) return 0;
    }
    return *a == *b;
}

static int lower_prefix(const char *s, const char *p) {
    for (; *p; s++, p++) {
        char x = (*s >= 'A' && *s <= 'Z') ? (char)(*s + 32) : *s;
        if (x != *p) return 0;
    }
    return 1;
}

static int family_of(const char *dll) {
    if (lower_eq(dll, "kernel32.dll") || lower_eq(dll, "kernelbase.dll") || lower_prefix(dll, "api-ms-win-core-")) return FAM_KERNEL;
    if (lower_eq(dll, "winmm.dll") || lower_eq(dll, "winmmbase.dll")) return FAM_WINMM;
    if (lower_eq(dll, "d3d9.dll")) return FAM_D3D9;
    if (lower_eq(dll, "d3d8.dll")) return FAM_D3D8;
    if (lower_eq(dll, "dxgi.dll")) return FAM_DXGI;
    if (lower_eq(dll, "d3d11.dll")) return FAM_D3D11;
    if (lower_eq(dll, "d3d10.dll") || lower_eq(dll, "d3d10_1.dll")) return FAM_D3D10;
    if (lower_eq(dll, "gdi32.dll")) return FAM_GDI;
    if (lower_eq(dll, "opengl32.dll")) return FAM_GL;
    if (lower_eq(dll, "vulkan-1.dll")) return FAM_VK;
    return FAM_NONE;
}

static void *real_clock_fn(const target *t) {
    HMODULE m = NULL;
    if (t->fam == FAM_KERNEL) m = GetModuleHandleA("kernel32.dll");
    else if (t->fam == FAM_WINMM) m = (g_real && lower_eq(ss_proxy, "winmm")) ? g_real : LoadLibraryA("winmm.dll");
    return m ? (void *)GetProcAddress(m, t->name) : NULL;
}

/* The originals of clock functions come from the real DLLs, never from an import slot (a
 * delay-load slot holds the loader's thunk until first use). */
static int ensure_clock_orig(target *t) {
    if (*t->orig) return 1;
    void *p = real_clock_fn(t);
    if (!p) return 0;
    *t->orig = p;
    if (t->orig == (void **)&o_timeGetTime && !g_mm_origin_set) {
        g_mm_origin = o_timeGetTime();
        g_mm_origin_set = 1;
    }
    return 1;
}

typedef struct {
    HMODULE h;
    int clock, present;
    char name[64];
} modinfo;
#define MAX_MODS 512
static modinfo g_mods[MAX_MODS];
static int g_nmods;

static const modinfo *mod_of(HMODULE h) {
    for (int i = 0; i < g_nmods; i++)
        if (g_mods[i].h == h) return &g_mods[i];
    return NULL;
}

static int patch_slot(void **slot, void *value) {
    DWORD old;
    if (!VirtualProtect(slot, sizeof(void *), PAGE_READWRITE, &old)) return 0;
    __atomic_store_n(slot, value, __ATOMIC_RELEASE);
    VirtualProtect(slot, sizeof(void *), old, &old);
    InterlockedIncrement(&g_patched_slots);
    return 1;
}

/* One import: `slot` of `fn` from `dll` in module `mi`. */
static void consider(const modinfo *mi, const char *dll, const char *fn, void **slot, int delayed) {
    int fam = family_of(dll);
    if (fam == FAM_NONE) return;
    for (int i = 0; i < NTARGETS; i++) {
        target *t = &TARGETS[i];
        if (t->fam != fam || strcmp(t->name, fn)) continue;
        if (t->kind == KIND_CLOCK && !mi->clock) return;
        if (t->kind == KIND_PRESENT && (!mi->present || delayed)) return;
        void *detour = t->detour;
        if (t->kind == KIND_GPA) {
            extern FARPROC WINAPI ss_GetProcAddress(HMODULE, LPCSTR);
            detour = (void *)ss_GetProcAddress;
            if (!*t->orig) *t->orig = (void *)GetProcAddress; /* this DLL's own import: the real one */
        }
        if (*slot == detour) return;
        if (t->kind == KIND_CLOCK) {
            if (!ensure_clock_orig(t)) {
                ss_log("  %s: %s!%s: no real %s found; left alone", mi->name, dll, fn, fn);
                return;
            }
        } else if (t->kind == KIND_PRESENT) {
            if (!*t->orig) *t->orig = *slot;
            else if (*t->orig != *slot) {
                ss_log("  %s: %s!%s points elsewhere (%p, not %p); left alone", mi->name, dll, fn, *slot, *t->orig);
                return;
            }
        }
        if (patch_slot(slot, detour)) {
            ss_log("  %s: %s!%s%s slot %p patched", mi->name, dll, fn, delayed ? " (delay-load)" : "", (void *)slot);
            if (t->kind != KIND_GPA) note_hooked(fn);
        }
        return;
    }
}

typedef struct {
    DWORD attrs, dll_name, hmod, iat, int_, bound_iat, unload_iat, timestamp;
} delay_descr;

static void patch_module(const modinfo *mi) {
    BYTE *base = (BYTE *)mi->h;
    IMAGE_DOS_HEADER *dos = (IMAGE_DOS_HEADER *)base;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) return;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE || nt->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR_MAGIC) return;
    IMAGE_DATA_DIRECTORY *dirs = nt->OptionalHeader.DataDirectory;
    if (dirs[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress && dirs[IMAGE_DIRECTORY_ENTRY_IMPORT].Size) {
        IMAGE_IMPORT_DESCRIPTOR *imp = (IMAGE_IMPORT_DESCRIPTOR *)(base + dirs[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress);
        for (; imp->Name; imp++) {
            const char *dll = (const char *)(base + imp->Name);
            if (family_of(dll) == FAM_NONE || !imp->FirstThunk) continue;
            IMAGE_THUNK_DATA *names = (IMAGE_THUNK_DATA *)(base + (imp->OriginalFirstThunk ? imp->OriginalFirstThunk : imp->FirstThunk));
            IMAGE_THUNK_DATA *slots = (IMAGE_THUNK_DATA *)(base + imp->FirstThunk);
            if (!imp->OriginalFirstThunk) {
                /* Bound without names: match each slot's value against the real exports. */
                HMODULE dm = GetModuleHandleA(dll);
                for (int k = 0; slots[k].u1.Function; k++)
                    for (int i = 0; dm && i < NTARGETS; i++)
                        if (TARGETS[i].fam == family_of(dll) && (void *)(uintptr_t)slots[k].u1.Function == (void *)GetProcAddress(dm, TARGETS[i].name))
                            consider(mi, dll, TARGETS[i].name, (void **)&slots[k].u1.Function, 0);
                continue;
            }
            for (int k = 0; names[k].u1.AddressOfData; k++) {
                if (IMAGE_SNAP_BY_ORDINAL(names[k].u1.Ordinal)) continue;
                IMAGE_IMPORT_BY_NAME *ibn = (IMAGE_IMPORT_BY_NAME *)(base + names[k].u1.AddressOfData);
                consider(mi, dll, (const char *)ibn->Name, (void **)&slots[k].u1.Function, 0);
            }
        }
    }
    /* Delay-load imports: clock functions only (their originals come from the real DLLs); a
     * delay-loaded present source is caught by the GetProcAddress hook the loader helper calls. */
    if (mi->clock && dirs[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT].VirtualAddress && dirs[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT].Size) {
        delay_descr *d = (delay_descr *)(base + dirs[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT].VirtualAddress);
        for (; d->dll_name; d++) {
            if (!(d->attrs & 1) || !d->iat || !d->int_) continue; /* RVA-based descriptors only */
            const char *dll = (const char *)(base + d->dll_name);
            if (family_of(dll) == FAM_NONE) continue;
            IMAGE_THUNK_DATA *names = (IMAGE_THUNK_DATA *)(base + d->int_);
            IMAGE_THUNK_DATA *slots = (IMAGE_THUNK_DATA *)(base + d->iat);
            for (int k = 0; names[k].u1.AddressOfData; k++) {
                if (IMAGE_SNAP_BY_ORDINAL(names[k].u1.Ordinal)) continue;
                IMAGE_IMPORT_BY_NAME *ibn = (IMAGE_IMPORT_BY_NAME *)(base + names[k].u1.AddressOfData);
                consider(mi, dll, (const char *)ibn->Name, (void **)&slots[k].u1.Function, 1);
            }
        }
    }
}

/* Names looked up at run time get the same answer as imports. */
FARPROC WINAPI ss_GetProcAddress(HMODULE m, LPCSTR name) {
    FARPROC real = o_GetProcAddress(m, name);
    if (!real || !g_on || IS_INTRESOURCE(name)) return real;
    HMODULE caller = NULL;
    GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, (LPCSTR)__builtin_return_address(0), &caller);
    char path[MAX_PATH];
    DWORD n = GetModuleFileNameA(m, path, sizeof path);
    if (!n || n >= sizeof path) return real;
    const char *base = strrchr(path, '\\');
    base = base ? base + 1 : path;
    int fam = family_of(base);
    if (fam == FAM_NONE) return real;
    EnterCriticalSection(&g_cs);
    const modinfo *mi = mod_of(caller);
    int clock = mi ? mi->clock : 0, present = mi ? mi->present : 0;
    LeaveCriticalSection(&g_cs);
    for (int i = 0; i < NTARGETS; i++) {
        target *t = &TARGETS[i];
        if (t->fam != fam || strcmp(t->name, name)) continue;
        if (t->kind == KIND_CLOCK) {
            if (!clock || !ensure_clock_orig(t)) return real;
            return (FARPROC)t->detour;
        }
        if (t->kind == KIND_PRESENT) {
            if (!present) return real;
            /* One original per name: a second, different one (another module's export of the
             * same name) is passed through unhooked. */
            if (!*t->orig) *t->orig = (void *)real;
            if (*t->orig != (void *)real) return real;
            note_hooked(name);
            return (FARPROC)t->detour;
        }
        if (t->kind == KIND_GPA) return clock || present ? (FARPROC)ss_GetProcAddress : real;
    }
    return real;
}

static int in_module_list(const char *name) {
    const char *p = g_cfg.modules;
    size_t n = strlen(name);
    while (*p) {
        const char *e = strchr(p, ';');
        size_t len = e ? (size_t)(e - p) : strlen(p);
        if (len == n && !strncmp(p, name, n)) return 1;
        if (!e) break;
        p = e + 1;
    }
    return 0;
}

/* A graphics or translation layer in the game's folder (a mod's dxgi.dll, an app-local DXVK):
 * never a present source of its own (its presents are the game's, already counted). */
static int is_layer_name(const char *n) {
    static const char *const L[] = {"d3d8.dll", "d3d9.dll", "d3d10.dll", "d3d10_1.dll", "d3d10core.dll", "d3d11.dll", "d3d12.dll",
                                    "d3d12core.dll", "dxgi.dll", "ddraw.dll", "opengl32.dll", "vulkan-1.dll", "nvapi.dll",
                                    "nvapi64.dll", "dxvk_d3d11.dll", "dxvk_dxgi.dll"};
    for (size_t i = 0; i < sizeof L / sizeof L[0]; i++)
        if (!strcmp(n, L[i])) return 1;
    return !strncmp(n, "nvngx", 5) || !strncmp(n, "amd_fidelityfx", 14) || !strncmp(n, "libxess", 7);
}

/* New modules since the last scan: classify, patch. */
static void scan_modules(void) {
    HMODULE mods[1024];
    DWORD need = 0;
    if (!K32EnumProcessModules(GetCurrentProcess(), mods, sizeof mods, &need)) return;
    DWORD count = need / sizeof(HMODULE);
    if (count > 1024) count = 1024;
    for (DWORD i = 0; i < count; i++) {
        HMODULE h = mods[i];
        EnterCriticalSection(&g_cs);
        int known = mod_of(h) != NULL || g_nmods >= MAX_MODS;
        LeaveCriticalSection(&g_cs);
        if (known || h == g_self || h == g_real) continue;
        WCHAR wpath[MAX_PATH];
        DWORD n = GetModuleFileNameW(h, wpath, MAX_PATH);
        if (!n || n >= MAX_PATH) continue;
        lower_w(wpath, n);
        const WCHAR *wb = wcsrchr(wpath, L'\\');
        wb = wb ? wb + 1 : wpath;
        modinfo mi;
        memset(&mi, 0, sizeof mi);
        mi.h = h;
        WideCharToMultiByte(CP_UTF8, 0, wb, -1, mi.name, sizeof mi.name - 1, NULL, NULL);
        int in_folder = !wcsncmp(wpath, g_exe_dir, wcslen(g_exe_dir));
        mi.clock = h == g_exe || in_module_list(mi.name);
        mi.present = mi.clock || (in_folder && !is_layer_name(mi.name));
        EnterCriticalSection(&g_cs);
        g_mods[g_nmods++] = mi;
        const modinfo *stored = &g_mods[g_nmods - 1];
        LeaveCriticalSection(&g_cs);
        if (stored->clock || stored->present) {
            ss_log("module %s: %s", stored->name, stored->clock ? "clock and present sources" : "present sources only");
            patch_module(stored);
        }
    }
}

/* ------------------------------------------------------------------------------------------ */
/* Start-up */

static DWORD WINAPI status_thread(void *arg) {
    (void)arg;
    int tick = 0;
    while (!g_quit) {
        Sleep(250); /* this DLL's own import: real time */
        if (g_quit) break;
        scan_modules();
        if (tick % 2 == 0) write_status("periodic");
        if (tick % 40 == 39) report("report");
        tick++;
    }
    return 0;
}

static void lower_ascii(char *s) {
    for (; *s; s++)
        if (*s >= 'A' && *s <= 'Z') *s = (char)(*s + 32);
}

static uint32_t env_u32(const char *name, uint32_t dflt) {
    char buf[32];
    DWORD n = GetEnvironmentVariableA(name, buf, sizeof buf);
    if (!n || n >= sizeof buf) return dflt;
    return (uint32_t)strtoul(buf, NULL, 10);
}

static int read_config(void) {
    char buf[64];
    DWORD n = GetEnvironmentVariableA("QUIVER_VCLOCK", buf, sizeof buf);
    if (!n || n >= sizeof buf) return 0;
    lower_ascii(buf);
    if (!strcmp(buf, "off") || !strcmp(buf, "0")) return 0;
    g_cfg.rate = strtod(buf, NULL);
    if (!(g_cfg.rate > 0.0)) return 0;
    g_cfg.creep_after = env_u32("QUIVER_VCLOCK_CREEP_AFTER", 4096);
    g_cfg.stuck_ms = env_u32("QUIVER_VCLOCK_STUCK_MS", 1000);
    g_cfg.sleep_max_ms = env_u32("QUIVER_VCLOCK_SLEEP_MAX_MS", 0);
    char sm[16];
    n = GetEnvironmentVariableA("QUIVER_VCLOCK_SLEEPS", sm, sizeof sm);
    g_cfg.sleeps = 1;
    if (n && n < sizeof sm) {
        lower_ascii(sm);
        g_cfg.sleeps = !strcmp(sm, "all") ? 2 : !strcmp(sm, "none") ? 0 : 1;
    }
    n = GetEnvironmentVariableA("QUIVER_VCLOCK_EXE", g_cfg.exe, sizeof g_cfg.exe);
    if (!n || n >= sizeof g_cfg.exe) g_cfg.exe[0] = 0;
    lower_ascii(g_cfg.exe);
    g_cfg.sync0 = env_u32("QUIVER_VCLOCK_SYNC0", 1) != 0;
    n = GetEnvironmentVariableA("QUIVER_VCLOCK_MODULES", g_cfg.modules, sizeof g_cfg.modules);
    if (n >= sizeof g_cfg.modules) g_cfg.modules[0] = 0;
    for (char *p = g_cfg.modules; *p; p++)
        if (*p == ',' || *p == ' ') *p = ';';
    lower_ascii(g_cfg.modules);
    n = GetEnvironmentVariableA("QUIVER_VCLOCK_DIR", g_cfg.dir, sizeof g_cfg.dir);
    if (!n || n >= sizeof g_cfg.dir) n = GetEnvironmentVariableA("QUIVER_LAB_RUN_DIR", g_cfg.dir, sizeof g_cfg.dir);
    if (!n || n >= sizeof g_cfg.dir) g_cfg.dir[0] = 0;
    return 1;
}

static void load_real(void) {
    char path[MAX_PATH];
    UINT n = GetSystemDirectoryA(path, MAX_PATH - 32);
    if (!n || n >= MAX_PATH - 32) return;
    snprintf(path + n, MAX_PATH - n, "\\%s.dll", ss_proxy);
    g_real = LoadLibraryA(path);
    for (int i = 0; i < ss_count; i++)
        ss_real[i] = g_real ? (void *)GetProcAddress(g_real, ss_names[i] ? ss_names[i] : (LPCSTR)(uintptr_t)ss_ords[i]) : NULL;
}

static void start(void) {
    g_exe = GetModuleHandleW(NULL);
    WCHAR exe[MAX_PATH];
    DWORD n = GetModuleFileNameW(NULL, exe, MAX_PATH);
    if (n && n < MAX_PATH) {
        lower_w(exe, n);
        WCHAR *slash = wcsrchr(exe, L'\\');
        if (g_cfg.exe[0]) {
            /* QUIVER_VCLOCK_EXE names the game's executable: other processes that load the
             * proxy (a crash handler, a launcher) still run the clock, but don't report
             * unless they present. */
            char base[MAX_PATH];
            WideCharToMultiByte(CP_UTF8, 0, slash ? slash + 1 : exe, -1, base, sizeof base, NULL, NULL);
            lower_ascii(base);
            g_main = !strcmp(base, g_cfg.exe);
        }
        if (slash) slash[1] = 0;
        lstrcpynW(g_exe_dir, exe, MAX_PATH);
    }
    HMODULE k32 = GetModuleHandleA("kernel32.dll");
    p_real_qpc = (void *)GetProcAddress(k32, "QueryPerformanceCounter");
    p_real_qpf = (void *)GetProcAddress(k32, "QueryPerformanceFrequency");
    p_real_gtc64 = (void *)GetProcAddress(k32, "GetTickCount64");
    LARGE_INTEGER f;
    if (!p_real_qpc || !p_real_qpf || !p_real_qpf(&f) || f.QuadPart <= 0) return;
    if (!vc_init(&g_clock, g_cfg.rate, f.QuadPart, g_cfg.creep_after, g_cfg.stuck_ms, real_now, NULL)) return;
    if (g_cfg.dir[0]) {
        char path[MAX_PATH + 32];
        snprintf(path, sizeof path, "%s\\vclock.log", g_cfg.dir);
        g_log = CreateFileA(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    }
    g_tick_origin = GetTickCount();
    g_tick64_origin = p_real_gtc64 ? p_real_gtc64() : g_tick_origin;
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    g_ft_origin = ((uint64_t)ft.dwHighDateTime << 32) | ft.dwLowDateTime;
    g_on = 1;

    char exe8[MAX_PATH * 2];
    WideCharToMultiByte(CP_UTF8, 0, g_exe_dir, -1, exe8, sizeof exe8, NULL, NULL);
    WCHAR self[MAX_PATH];
    char self8[MAX_PATH * 2] = "?";
    DWORD sn = GetModuleFileNameW(NULL, self, MAX_PATH);
    if (sn && sn < MAX_PATH) WideCharToMultiByte(CP_UTF8, 0, self, -1, self8, sizeof self8, NULL, NULL);
    ss_log("--- quiver virtual clock (%s.dll proxy, %s) in pid %lu, %s, game folder %s", ss_proxy, sizeof(void *) == 8 ? "x64" : "x86",
           (unsigned long)GetCurrentProcessId(), self8, exe8);
    if (!g_main)
        ss_log("this isn't the game's executable (%s): the clock runs here too, but this process reports only if it presents", g_cfg.exe);
    double period_ms = 1000.0 / g_cfg.rate;
    ss_log("virtual clock: %.4f Hz -> %lld ticks per frame at a reported QPF of %lld (real QPF %lld); a frame is %.9f s (%.4f ms)",
           g_cfg.rate, (long long)g_clock.ticks_per_frame, (long long)g_clock.vfreq, (long long)g_clock.real_freq,
           (double)g_clock.ticks_per_frame / (double)g_clock.vfreq, period_ms);
    if ((double)g_clock.vfreq != (double)g_clock.ticks_per_frame * g_cfg.rate)
        ss_log("WARNING: %.4f Hz doesn't divide the counter into whole ticks: frame deltas are %lld/%lld s, not exactly 1/%.4f",
               g_cfg.rate, (long long)g_clock.ticks_per_frame, (long long)g_clock.vfreq, g_cfg.rate);
    if (period_ms - (double)(long long)period_ms > 1e-9)
        ss_log("WARNING: a %.4f ms frame isn't a whole number of milliseconds: games timing frames with timeGetTime or "
               "GetTickCount see %lld and %lld ms frames alternate. 125 Hz (8 ms), 100 Hz (10 ms) or 62.5 Hz (16 ms) are exact.",
               period_ms, (long long)period_ms, (long long)period_ms + 1);
    ss_log("creep after %u reads by one thread in a frame (%lld ticks each), real-time escape after %u ms; sleeps shortened "
           "(to at most %u ms real) on %s; present sync interval forced to 0: %s; extra clock modules: %s",
           (unsigned)g_clock.creep_after, (long long)g_clock.creep_per_query, g_clock.stuck_ms, g_cfg.sleep_max_ms,
           g_cfg.sleeps == 2 ? "every thread" : g_cfg.sleeps == 1 ? "the presenting thread only" : "no thread",
           g_cfg.sync0 ? "yes" : "no", g_cfg.modules[0] ? g_cfg.modules : "none");
    for (int i = 0; i < ss_count; i++)
        if (!ss_real[i]) ss_log("WARNING: the real %s.dll has no export %s (ordinal %u): calling it would crash", ss_proxy,
                                ss_names[i] ? ss_names[i] : "-", (unsigned)ss_ords[i]);
    ss_log("audio, and anything else on a clock this can't reach, drifts: a testing tool, never for play");
    scan_modules();
    write_status("installed");
    HANDLE t = CreateThread(NULL, 0, status_thread, NULL, 0, NULL);
    if (t) CloseHandle(t);
}

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved) {
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH) {
        g_self = inst;
        DisableThreadLibraryCalls(inst);
        InitializeCriticalSection(&g_cs);
        InitializeCriticalSection(&g_census_cs);
        g_tls = TlsAlloc();
        g_tls_thr = TlsAlloc();
        load_real();
        if (g_tls != TLS_OUT_OF_INDEXES && g_tls_thr != TLS_OUT_OF_INDEXES && read_config()) start();
    } else if (reason == DLL_PROCESS_DETACH && g_on) {
        g_quit = 1;
        report("exit");
        write_status("exit");
    }
    return TRUE;
}
