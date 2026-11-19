/* A stand-in game for tests/wine.rs: presents frames through Direct3D 11 or 9 (DXVK) and reads
 * the clocks a game reads, through its own imports (so through the virtual clock when it's on).
 * Real time comes from ntdll's NtQueryPerformanceCounter, which the clock never touches.
 *
 *   clock_test.exe <d3d11|d3d11dev|d3d9> <frames> <slow_ms> <title> [helper.exe]
 *   helper.exe child                  (a second process loading the proxy that never presents)
 *
 * d3d11 makes its swap chain with D3D11CreateDeviceAndSwapChain; d3d11dev with D3D11CreateDevice
 * and the factory it gets from the device (IDXGIDevice -> adapter -> parent), as Unity does.
 * With a helper path, that copy of this exe is started first (a crash handler's place).
 * Background threads run throughout: one polls the clock every Sleep(1), one every Sleep(100),
 * as a game's timer and streaming threads do.
 *
 * slow_ms > 0 makes every frame take at least that long in real time (NtDelayExecution, not
 * Sleep, which the clock shortens): a presenter slower than the virtual rate.
 *
 * Phase A: <frames> frames, each starting with a read of QueryPerformanceCounter, timeGetTime,
 * GetTickCount and GetTickCount64. Phase B: 30 frames, each waiting in a QPC busy-wait for two
 * frame periods (it can only ever see one per Present: must terminate through the creep).
 * Phase C: 30 frames waiting with `while (timeGetTime() - t0 < 2 periods) Sleep(1)`. Phase D:
 * 10 frames, each waiting on a worker thread that busy-waits on QPC for two periods while this
 * thread is blocked (no Present can come). Then, for
 * every loaded module, how many of its clock imports point into the hook DLL.
 *
 * With QUIVER_TEST_KEYS set (the stepped case, which pauses and steps it through the control
 * page while the test presses keys in the Xvfb), phase A also samples the Right arrow at the
 * start of every frame as window messages, raw input and GetAsyncKeyState see it, and prints
 * the frames each saw it down. */
#define COBJMACROS
#define CINTERFACE
#define PSAPI_VERSION 2
#include <windows.h>
#include <psapi.h>
#include <d3d11.h>
#include <d3d9.h>
#include <mmsystem.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

typedef LONG(NTAPI *pfn_ntqpc)(LARGE_INTEGER *, LARGE_INTEGER *);
typedef LONG(NTAPI *pfn_ntdelay)(BOOLEAN, LARGE_INTEGER *);
static pfn_ntqpc nt_qpc;
static pfn_ntdelay nt_delay;
static LONGLONG real_freq;

static double real_s(void) {
    LARGE_INTEGER t, f;
    nt_qpc(&t, &f);
    return (double)t.QuadPart / (double)real_freq;
}

static void real_delay_ms(int ms) {
    LARGE_INTEGER d;
    d.QuadPart = -(LONGLONG)ms * 10000;
    nt_delay(FALSE, &d);
}

/* Distinct values of a sequence of steps. */
#define NB 16
typedef struct { long long v[NB]; long c[NB]; int n; long other; } census;
static void cadd(census *c, long long v) {
    for (int i = 0; i < c->n; i++)
        if (c->v[i] == v) { c->c[i]++; return; }
    if (c->n < NB) { c->v[c->n] = v; c->c[c->n++] = 1; } else c->other++;
}
static void cprint(const char *tag, census *c) {
    printf("%s", tag);
    for (int i = 0; i < c->n; i++) printf(" %lld:%ld", c->v[i], c->c[i]);
    if (c->other) printf(" other:%ld", c->other);
    printf("\n");
}

static IDXGISwapChain *g_sc;
static ID3D11DeviceContext *g_ctx;
static ID3D11RenderTargetView *g_rtv;
static IDirect3DDevice9 *g_d9;
static int g_d3d9;

static void frame(int i) {
    float col[4] = {(float)(i % 64) / 64.0f, 0.2f, 0.4f, 1.0f};
    if (g_d3d9) {
        IDirect3DDevice9_Clear(g_d9, 0, NULL, D3DCLEAR_TARGET, D3DCOLOR_XRGB(i & 255, 40, 90), 1.0f, 0);
        IDirect3DDevice9_Present(g_d9, NULL, NULL, NULL, NULL);
    } else {
        ID3D11DeviceContext_ClearRenderTargetView(g_ctx, g_rtv, col);
        IDXGISwapChain_Present(g_sc, 1, 0); /* sync interval 1: the clock forces 0 */
    }
    MSG m;
    while (PeekMessageA(&m, NULL, 0, 0, PM_REMOVE)) DispatchMessageA(&m);
}

static int hooked_slots(HMODULE mod, BYTE *lo, BYTE *hi) {
    BYTE *base = (BYTE *)mod;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    if (nt->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR_MAGIC) return 0;
    IMAGE_DATA_DIRECTORY d = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!d.VirtualAddress) return 0;
    int n = 0;
    for (IMAGE_IMPORT_DESCRIPTOR *imp = (IMAGE_IMPORT_DESCRIPTOR *)(base + d.VirtualAddress); imp->Name; imp++) {
        const char *dll = (const char *)(base + imp->Name);
        /* Clock families only: a module importing version.dll legitimately points at the proxy. */
        if (_strnicmp(dll, "kernel32", 8) && _strnicmp(dll, "kernelbase", 10) && _strnicmp(dll, "api-ms-win-core", 15) && _strnicmp(dll, "winmm", 5))
            continue;
        IMAGE_THUNK_DATA *s = (IMAGE_THUNK_DATA *)(base + imp->FirstThunk);
        for (int k = 0; s[k].u1.Function; k++) {
            BYTE *p = (BYTE *)(uintptr_t)s[k].u1.Function;
            if (p >= lo && p < hi) n++;
        }
    }
    return n;
}

static volatile LONG g_stop;
static DWORD WINAPI poller(void *arg) {
    DWORD ms = (DWORD)(uintptr_t)arg;
    while (!g_stop) {
        Sleep(ms);
        LARGE_INTEGER t;
        QueryPerformanceCounter(&t);
        timeGetTime();
    }
    return 0;
}

static LONGLONG g_wait_ticks;
static DWORD WINAPI spinner(void *arg) {
    (void)arg;
    LARGE_INTEGER a, b;
    QueryPerformanceCounter(&a);
    do QueryPerformanceCounter(&b);
    while (b.QuadPart - a.QuadPart < g_wait_ticks);
    return 0;
}

/* QUIVER_TEST_KEYS: the Right arrow as three input APIs see it, sampled at the start of every
 * phase A frame: window messages (WM_KEYDOWN/UP), raw input (WM_INPUT) and the polled state
 * (GetAsyncKeyState). */
static int g_keys, g_msg_down, g_raw_down, g_keydowns, g_raw_makes;
static LRESULT CALLBACK wndproc(HWND w, UINT m, WPARAM a, LPARAM b) {
    if (g_keys && (m == WM_KEYDOWN || m == WM_KEYUP) && a == VK_RIGHT) {
        if (m == WM_KEYDOWN && !g_msg_down) g_keydowns++;
        g_msg_down = m == WM_KEYDOWN;
    }
    if (g_keys && m == WM_INPUT) {
        RAWINPUT ri;
        UINT sz = sizeof ri;
        if (GetRawInputData((HRAWINPUT)b, RID_INPUT, &ri, &sz, sizeof(RAWINPUTHEADER)) != (UINT)-1 && ri.header.dwType == RIM_TYPEKEYBOARD &&
            ri.data.keyboard.VKey == VK_RIGHT) {
            int down = !(ri.data.keyboard.Flags & RI_KEY_BREAK);
            if (down && !g_raw_down) g_raw_makes++;
            g_raw_down = down;
        }
    }
    return DefWindowProcA(w, m, a, b);
}

typedef struct { int frames, first, last; } span;
static void span_add(span *s, int on, int i) {
    if (!on) return;
    if (!s->frames) s->first = i;
    s->frames++;
    s->last = i;
}

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "child")) {
        /* A process that loads the proxy, reads the clock and never presents. */
        LARGE_INTEGER t;
        for (int i = 0; i < 200; i++) {
            QueryPerformanceCounter(&t);
            Sleep(10);
        }
        return 0;
    }
    if (argc < 5) return 2;
    g_d3d9 = !strcmp(argv[1], "d3d9");
    int devfactory = !strcmp(argv[1], "d3d11dev");
    if (argc >= 6) {
        STARTUPINFOA si = {sizeof si};
        PROCESS_INFORMATION pi;
        char cmd[MAX_PATH + 16];
        snprintf(cmd, sizeof cmd, "\"%s\" child", argv[5]);
        if (CreateProcessA(argv[5], cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
            printf("helper pid %lu\n", (unsigned long)pi.dwProcessId);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        } else printf("helper failed %lu\n", (unsigned long)GetLastError());
    }
    int frames = atoi(argv[2]), slow = atoi(argv[3]);
    setvbuf(stdout, NULL, _IONBF, 0);
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    nt_qpc = (pfn_ntqpc)GetProcAddress(ntdll, "NtQueryPerformanceCounter");
    nt_delay = (pfn_ntdelay)GetProcAddress(ntdll, "NtDelayExecution");
    LARGE_INTEGER t0, rf;
    nt_qpc(&t0, &rf);
    real_freq = rf.QuadPart;

    /* Import version.dll (the proxy) for real. */
    DWORD h;
    char self[MAX_PATH];
    GetModuleFileNameA(NULL, self, MAX_PATH);
    GetFileVersionInfoSizeA(self, &h);

    LARGE_INTEGER qpf;
    QueryPerformanceFrequency(&qpf);
    printf("qpf %lld real_qpf %lld\n", (long long)qpf.QuadPart, (long long)real_freq);

    WNDCLASSA wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "quiver-vclock-test";
    RegisterClassA(&wc);
    HWND wnd = CreateWindowA(wc.lpszClassName, argv[4], WS_POPUP | WS_VISIBLE, 0, 0, 160, 120, NULL, NULL, wc.hInstance, NULL);
    if (g_d3d9) {
        IDirect3D9 *d3d = Direct3DCreate9(D3D_SDK_VERSION);
        D3DPRESENT_PARAMETERS pp = {0};
        pp.Windowed = TRUE;
        pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
        pp.BackBufferFormat = D3DFMT_X8R8G8B8;
        pp.BackBufferWidth = 160;
        pp.BackBufferHeight = 120;
        pp.hDeviceWindow = wnd;
        pp.PresentationInterval = D3DPRESENT_INTERVAL_ONE;
        if (!d3d || FAILED(IDirect3D9_CreateDevice(d3d, 0, D3DDEVTYPE_HAL, wnd, D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &g_d9))) {
            printf("no d3d9 device\n");
            return 3;
        }
    } else {
        DXGI_SWAP_CHAIN_DESC sd = {0};
        sd.BufferCount = 2;
        sd.BufferDesc.Width = 160;
        sd.BufferDesc.Height = 120;
        sd.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
        sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        sd.OutputWindow = wnd;
        sd.SampleDesc.Count = 1;
        sd.Windowed = TRUE;
        sd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
        ID3D11Device *dev;
        if (devfactory) {
            IDXGIDevice *dd;
            IDXGIAdapter *ad;
            IDXGIFactory *f;
            if (FAILED(D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &dev, NULL, &g_ctx)) ||
                FAILED(ID3D11Device_QueryInterface(dev, &IID_IDXGIDevice, (void **)&dd)) || FAILED(IDXGIDevice_GetAdapter(dd, &ad)) ||
                FAILED(IDXGIAdapter_GetParent(ad, &IID_IDXGIFactory, (void **)&f)) ||
                FAILED(IDXGIFactory_CreateSwapChain(f, (IUnknown *)dev, &sd, &g_sc))) {
                printf("no d3d11 device or swap chain (device factory)\n");
                return 3;
            }
        } else if (FAILED(D3D11CreateDeviceAndSwapChain(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &sd, &g_sc, &dev, NULL, &g_ctx))) {
            printf("no d3d11 device\n");
            return 3;
        }
        ID3D11Texture2D *bb;
        IDXGISwapChain_GetBuffer(g_sc, 0, &IID_ID3D11Texture2D, (void **)&bb);
        ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)bb, NULL, &g_rtv);
        ID3D11Texture2D_Release(bb);
    }

    g_keys = getenv("QUIVER_TEST_KEYS") != NULL;
    if (g_keys) {
        RAWINPUTDEVICE rid = {1, 6, 0, wnd}; /* generic desktop, keyboard */
        printf("raw_input %d\n", RegisterRawInputDevices(&rid, 1, sizeof rid) ? 1 : 0);
        SetForegroundWindow(wnd);
        SetFocus(wnd);
    }
    span k_msg = {0}, k_raw = {0}, k_async = {0};
    HANDLE bg1 = CreateThread(NULL, 0, poller, (void *)(uintptr_t)1, 0, NULL);
    HANDLE bg2 = CreateThread(NULL, 0, poller, (void *)(uintptr_t)100, 0, NULL);

    /* Phase A. */
    LONGLONG *q = calloc((size_t)frames, sizeof *q);
    DWORD *mm = calloc((size_t)frames, sizeof *mm), *tc = calloc((size_t)frames, sizeof *tc);
    ULONGLONG *tc64 = calloc((size_t)frames, sizeof *tc64);
    double ra = 0, rb = 0;
    int gpa_virtual = -1;
    typedef BOOL(WINAPI * pfn_qpc)(LARGE_INTEGER *);
    pfn_qpc gpa_qpc = (pfn_qpc)GetProcAddress(GetModuleHandleA("kernel32.dll"), "QueryPerformanceCounter");
    for (int i = 0; i < frames; i++) {
        LARGE_INTEGER t;
        QueryPerformanceCounter(&t);
        q[i] = t.QuadPart;
        mm[i] = timeGetTime();
        tc[i] = GetTickCount();
        tc64[i] = GetTickCount64();
        if (i == 1) ra = real_s();
        if (i == frames / 2) {
            /* Looked up at run time, inside one frame: the same virtual value. */
            LARGE_INTEGER a;
            gpa_qpc(&a);
            gpa_virtual = a.QuadPart == t.QuadPart;
        }
        if (g_keys) {
            span_add(&k_msg, g_msg_down, i);
            span_add(&k_raw, g_raw_down, i);
            span_add(&k_async, GetAsyncKeyState(VK_RIGHT) < 0, i);
        }
        if (slow > 0) real_delay_ms(slow);
        frame(i);
    }
    rb = real_s();
    census cq = {0}, cm = {0}, ct = {0}, ct64 = {0};
    unsigned long long hash = 1469598103934665603ull;
    for (int i = 2; i < frames; i++) {
        cadd(&cq, q[i] - q[i - 1]);
        cadd(&cm, (long long)(mm[i] - mm[i - 1]));
        cadd(&ct, (long long)(tc[i] - tc[i - 1]));
        cadd(&ct64, (long long)(tc64[i] - tc64[i - 1]));
    }
    for (int i = 1; i < frames; i++) {
        unsigned long long v = (unsigned long long)(q[i] - q[1]);
        for (int b = 0; b < 8; b++) hash = (hash ^ ((v >> (8 * b)) & 255)) * 1099511628211ull;
    }
    cprint("A qpc_steps", &cq);
    cprint("A ms_steps", &cm);
    cprint("A tick_steps", &ct);
    cprint("A tick64_steps", &ct64);
    printf("A frames %d virtual_s %.6f real_s %.6f\n", frames - 1, (double)(q[frames - 1] - q[1]) / (double)qpf.QuadPart, rb - ra);
    printf("A sequence_hash %016llx\n", hash);
    printf("gpa_virtual %d\n", gpa_virtual);
    if (g_keys) {
        printf("keys messages %d first %d last %d presses %d\n", k_msg.frames, k_msg.first, k_msg.last, g_keydowns);
        printf("keys raw %d first %d last %d presses %d\n", k_raw.frames, k_raw.first, k_raw.last, g_raw_makes);
        printf("keys async %d first %d last %d\n", k_async.frames, k_async.first, k_async.last);
    }

    /* Phase B: a QPC busy-wait for two frame periods. */
    LONGLONG period = qpf.QuadPart / 60; /* any period: the wait is for 2 virtual frames */
    {
        LARGE_INTEGER a;
        QueryPerformanceCounter(&a);
        LONGLONG f1 = a.QuadPart;
        frame(0);
        QueryPerformanceCounter(&a);
        period = a.QuadPart - f1; /* exactly one frame on the virtual clock */
    }
    double r0 = real_s();
    LARGE_INTEGER last;
    QueryPerformanceCounter(&last);
    long long spins = 0;
    for (int i = 0; i < 30; i++) {
        LARGE_INTEGER now;
        do {
            QueryPerformanceCounter(&now);
            spins++;
        } while (now.QuadPart - last.QuadPart < 2 * period);
        last = now;
        frame(i);
    }
    printf("B frames 30 spins %lld real_s %.6f period %lld\n", spins, real_s() - r0, (long long)period);

    /* Phase C: a Sleep(1) loop on timeGetTime for two frame periods. */
    DWORD period_ms = (DWORD)(period * 1000 / qpf.QuadPart);
    r0 = real_s();
    DWORD tl = timeGetTime();
    long long sleeps = 0;
    for (int i = 0; i < 30; i++) {
        DWORD now;
        while ((now = timeGetTime()) - tl < 2 * period_ms) {
            Sleep(1);
            sleeps++;
        }
        tl = now;
        frame(i);
    }
    printf("C frames 30 sleeps %lld real_s %.6f period_ms %lu\n", sleeps, real_s() - r0, (unsigned long)period_ms);

    /* Phase D: a worker busy-waits for two periods while this (presenting) thread is blocked. */
    g_wait_ticks = 2 * period;
    r0 = real_s();
    for (int i = 0; i < 10; i++) {
        HANDLE w = CreateThread(NULL, 0, spinner, NULL, 0, NULL);
        WaitForSingleObject(w, INFINITE);
        CloseHandle(w);
        frame(i);
    }
    printf("D frames 10 real_s %.6f\n", real_s() - r0);
    g_stop = 1;
    WaitForSingleObject(bg1, INFINITE);
    WaitForSingleObject(bg2, INFINITE);

    /* Which modules' clock imports point into the hook DLL. */
    HMODULE proxy = GetModuleHandleA("version.dll");
    MODULEINFO pi = {0};
    K32GetModuleInformation(GetCurrentProcess(), proxy, &pi, sizeof pi);
    BYTE *lo = (BYTE *)pi.lpBaseOfDll, *hi = lo + pi.SizeOfImage;
    HMODULE mods[512];
    DWORD need;
    K32EnumProcessModules(GetCurrentProcess(), mods, sizeof mods, &need);
    for (DWORD i = 0; i < need / sizeof(HMODULE) && i < 512; i++) {
        char name[MAX_PATH];
        K32GetModuleBaseNameA(GetCurrentProcess(), mods[i], name, sizeof name);
        printf("iat %s %d%s\n", name, hooked_slots(mods[i], lo, hi), mods[i] == GetModuleHandleA(NULL) ? " exe" : "");
    }
    printf("done\n");
    return 0;
}
