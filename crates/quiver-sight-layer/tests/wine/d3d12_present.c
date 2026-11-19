/* A minimal D3D12 game loop for tests/wine.rs: a window, a flip-model swapchain on a direct
 * queue, a known pattern per frame (left half red, right half blue, a green band whose
 * position follows the frame) and a Present. Under Proton this runs on vkd3d-proton, whose
 * presents reach the Sight layer as vkQueuePresentKHR calls.
 *
 *   d3d12_present.exe <frames> <title> [resize_at] [flags]
 *
 * flags (bits): 1 uncapped (no sleep between frames), 2 tearing (ALLOW_TEARING, presents
 * with sync interval 0 map to IMMEDIATE), 4 a 10-bit swapchain (R10G10B10A2_UNORM),
 * 8 sync interval 1 (FIFO), 16 the resize goes to exclusive fullscreen at 640x480.
 *
 * With resize_at > 0 the swapchain's buffers are resized (ResizeBuffers) at that frame, so
 * the Vulkan swapchain is recreated with oldSwapchain.
 *
 * Key and mouse-button messages are printed (KEYDOWN 0x41, KEYUP …, BUTTON L DOWN 0 x y), and
 * appended to the file QUIVER_PROBE_LOG names.
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -o d3d12_present.exe d3d12_present.c -ld3d12 -ldxgi -luuid -ldxguid
 */
#define COBJMACROS
#define WIDL_C_INLINE_WRAPPERS
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <stdio.h>
#include <stdlib.h>

#define BUFFERS 3

/* Key and mouse messages go to stdout, and to the file QUIVER_PROBE_LOG names (a Windows
 * path; `proton run` doesn't pass a program's stdout on): the lab test checks what reached
 * the game. */
static void input_line(const char *fmt, unsigned a, int x, int y)
{
    static FILE *f;
    static int tried;
    if (!tried) {
        const char *p = getenv("QUIVER_PROBE_LOG");
        tried = 1;
        if (p && *p)
            f = fopen(p, "a");
    }
    printf(fmt, a, x, y);
    printf("\n");
    fflush(stdout);
    if (f) {
        fprintf(f, fmt, a, x, y);
        fprintf(f, "\n");
        fflush(f);
    }
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    switch (m) {
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    case WM_KEYDOWN: case WM_SYSKEYDOWN:
        input_line("KEYDOWN 0x%02x", (unsigned)w, 0, 0);
        break;
    case WM_KEYUP: case WM_SYSKEYUP:
        input_line("KEYUP 0x%02x", (unsigned)w, 0, 0);
        break;
    case WM_LBUTTONDOWN:
        input_line("BUTTON L DOWN %u %d %d", 0, (short)LOWORD(l), (short)HIWORD(l));
        break;
    }
    return DefWindowProcW(h, m, w, l);
}

static ID3D12Device *dev;
static IDXGISwapChain3 *sc;
static ID3D12DescriptorHeap *heap;
static ID3D12Resource *bufs[BUFFERS];
static UINT rtv_size;

static void get_buffers(void)
{
    D3D12_CPU_DESCRIPTOR_HANDLE h;
    h = ID3D12DescriptorHeap_GetCPUDescriptorHandleForHeapStart(heap);
    for (int i = 0; i < BUFFERS; i++) {
        IDXGISwapChain3_GetBuffer(sc, i, &IID_ID3D12Resource, (void **)&bufs[i]);
        D3D12_CPU_DESCRIPTOR_HANDLE hi = {h.ptr + i * rtv_size};
        ID3D12Device_CreateRenderTargetView(dev, bufs[i], NULL, hi);
    }
}

int main(int argc, char **argv)
{
    int frames = argc > 1 ? atoi(argv[1]) : 600;
    const char *title = argc > 2 ? argv[2] : "quiver-sight-d3d12";
    int resize_at = argc > 3 ? atoi(argv[3]) : 0;
    int flags = argc > 4 ? atoi(argv[4]) : 0;
    UINT sc_flags = (flags & 2) ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0;
    UINT w = 1280, h = 720;
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"QuiverSightD3D12";
    RegisterClassW(&wc);
    /* Undecorated at (0, 0): the client area is the screen's top-left w x h (tests read it
     * back from the X server). */
    HWND hwnd = CreateWindowExA(0, "QuiverSightD3D12", title, WS_POPUP | WS_VISIBLE, 0, 0, (int)w, (int)h, NULL, NULL, wc.hInstance, NULL);
    input_line("START %u", 0, 0, 0);
    if (!hwnd) {
        fprintf(stderr, "CreateWindow failed\n");
        return 1;
    }

    IDXGIFactory4 *factory = NULL;
    HRESULT hr = CreateDXGIFactory1(&IID_IDXGIFactory4, (void **)&factory);
    if (FAILED(hr)) {
        fprintf(stderr, "CreateDXGIFactory1 failed: 0x%08lx\n", hr);
        return 2;
    }
    hr = D3D12CreateDevice(NULL, D3D_FEATURE_LEVEL_11_0, &IID_ID3D12Device, (void **)&dev);
    if (FAILED(hr)) {
        fprintf(stderr, "D3D12CreateDevice failed: 0x%08lx\n", hr);
        return 2;
    }
    D3D12_COMMAND_QUEUE_DESC qd = {0};
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ID3D12CommandQueue *queue = NULL;
    ID3D12Device_CreateCommandQueue(dev, &qd, &IID_ID3D12CommandQueue, (void **)&queue);

    DXGI_SWAP_CHAIN_DESC1 sd = {0};
    sd.Width = w;
    sd.Height = h;
    sd.Format = (flags & 4) ? DXGI_FORMAT_R10G10B10A2_UNORM : DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.Flags = sc_flags;
    sd.SampleDesc.Count = 1;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.BufferCount = BUFFERS;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    IDXGISwapChain1 *sc1 = NULL;
    hr = IDXGIFactory4_CreateSwapChainForHwnd(factory, (IUnknown *)queue, hwnd, &sd, NULL, NULL, &sc1);
    if (FAILED(hr)) {
        fprintf(stderr, "CreateSwapChainForHwnd failed: 0x%08lx\n", hr);
        return 3;
    }
    IDXGISwapChain1_QueryInterface(sc1, &IID_IDXGISwapChain3, (void **)&sc);
    IDXGISwapChain1_Release(sc1);

    D3D12_DESCRIPTOR_HEAP_DESC hd = {0};
    hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    hd.NumDescriptors = BUFFERS;
    ID3D12Device_CreateDescriptorHeap(dev, &hd, &IID_ID3D12DescriptorHeap, (void **)&heap);
    rtv_size = ID3D12Device_GetDescriptorHandleIncrementSize(dev, D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    get_buffers();

    ID3D12CommandAllocator *alloc[BUFFERS];
    for (int i = 0; i < BUFFERS; i++)
        ID3D12Device_CreateCommandAllocator(dev, D3D12_COMMAND_LIST_TYPE_DIRECT, &IID_ID3D12CommandAllocator, (void **)&alloc[i]);
    ID3D12GraphicsCommandList *list = NULL;
    ID3D12Device_CreateCommandList(dev, 0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc[0], NULL, &IID_ID3D12GraphicsCommandList, (void **)&list);
    ID3D12GraphicsCommandList_Close(list);
    ID3D12Fence *fence = NULL;
    ID3D12Device_CreateFence(dev, 0, D3D12_FENCE_FLAG_NONE, &IID_ID3D12Fence, (void **)&fence);
    UINT64 fence_values[BUFFERS] = {0};
    UINT64 next_value = 1;
    HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);

    for (int i = 0; i < frames; i++) {
        MSG msg;
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) {
                i = frames;
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if (resize_at > 0 && i == resize_at) {
            /* Wait for the GPU, drop the buffers, resize (a new Vulkan swapchain). */
            ID3D12CommandQueue_Signal(queue, fence, next_value);
            ID3D12Fence_SetEventOnCompletion(fence, next_value, event);
            WaitForSingleObject(event, INFINITE);
            next_value++;
            for (int k = 0; k < BUFFERS; k++) {
                ID3D12Resource_Release(bufs[k]);
                fence_values[k] = 0;
            }
            w = 1024;
            h = 640;
            if (flags & 16) {
                /* Exclusive fullscreen at 640x480, as a game set to that mode does. */
                DXGI_MODE_DESC m = {0};
                m.Width = w = 640;
                m.Height = h = 480;
                m.Format = sd.Format;
                hr = IDXGISwapChain3_ResizeTarget(sc, &m);
                if (SUCCEEDED(hr))
                    hr = IDXGISwapChain3_SetFullscreenState(sc, TRUE, NULL);
                if (FAILED(hr))
                    fprintf(stderr, "fullscreen failed: 0x%08lx\n", hr);
            }
            hr = IDXGISwapChain3_ResizeBuffers(sc, BUFFERS, w, h, DXGI_FORMAT_UNKNOWN, sc_flags);
            if (FAILED(hr)) {
                fprintf(stderr, "ResizeBuffers failed: 0x%08lx\n", hr);
                return 4;
            }
            get_buffers();
        }
        UINT bi = IDXGISwapChain3_GetCurrentBackBufferIndex(sc);
        if (ID3D12Fence_GetCompletedValue(fence) < fence_values[bi]) {
            ID3D12Fence_SetEventOnCompletion(fence, fence_values[bi], event);
            WaitForSingleObject(event, INFINITE);
        }
        ID3D12CommandAllocator_Reset(alloc[bi]);
        ID3D12GraphicsCommandList_Reset(list, alloc[bi], NULL);
        D3D12_RESOURCE_BARRIER b = {0};
        b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        b.Transition.pResource = bufs[bi];
        b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        b.Transition.StateBefore = D3D12_RESOURCE_STATE_PRESENT;
        b.Transition.StateAfter = D3D12_RESOURCE_STATE_RENDER_TARGET;
        ID3D12GraphicsCommandList_ResourceBarrier(list, 1, &b);
        D3D12_CPU_DESCRIPTOR_HANDLE rtv;
        rtv = ID3D12DescriptorHeap_GetCPUDescriptorHandleForHeapStart(heap);
        rtv.ptr += bi * rtv_size;
        float red[4] = {0.8f, 0.1f, 0.1f, 1.0f}, blue[4] = {0.1f, 0.1f, 0.8f, 1.0f}, green[4] = {0.1f, 0.8f, 0.1f, 1.0f};
        D3D12_RECT left = {0, 0, (LONG)w / 2, (LONG)h}, right = {(LONG)w / 2, 0, (LONG)w, (LONG)h};
        LONG y = (LONG)((i * 4) % (h - 40));
        D3D12_RECT band = {0, y, (LONG)w, y + 40};
        ID3D12GraphicsCommandList_ClearRenderTargetView(list, rtv, red, 1, &left);
        ID3D12GraphicsCommandList_ClearRenderTargetView(list, rtv, blue, 1, &right);
        ID3D12GraphicsCommandList_ClearRenderTargetView(list, rtv, green, 1, &band);
        b.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
        b.Transition.StateAfter = D3D12_RESOURCE_STATE_PRESENT;
        ID3D12GraphicsCommandList_ResourceBarrier(list, 1, &b);
        ID3D12GraphicsCommandList_Close(list);
        ID3D12CommandList *lists[1] = {(ID3D12CommandList *)list};
        ID3D12CommandQueue_ExecuteCommandLists(queue, 1, lists);
        hr = IDXGISwapChain3_Present(sc, (flags & 8) ? 1 : 0, (flags & 2) ? DXGI_PRESENT_ALLOW_TEARING : 0);
        if (FAILED(hr)) {
            fprintf(stderr, "Present failed at frame %d: 0x%08lx\n", i, hr);
            return 5;
        }
        ID3D12CommandQueue_Signal(queue, fence, next_value);
        fence_values[bi] = next_value++;
        if (!(flags & 1))
            Sleep(i % 150 == 75 ? 40 : 8);
    }
    ID3D12CommandQueue_Signal(queue, fence, next_value);
    ID3D12Fence_SetEventOnCompletion(fence, next_value, event);
    WaitForSingleObject(event, INFINITE);
    printf("presented %d frames\n", frames);
    fflush(stdout);
    return 0;
}
