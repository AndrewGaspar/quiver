/* A minimal D3D11 game loop for tests/wine.rs: a window, a flip-model swapchain, a clear and
 * a Present per frame. Under Proton this runs on DXVK, whose presents reach the Sight layer
 * as vkQueuePresentKHR calls.
 *
 *   d3d11_present.exe <frames> <title>
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -o d3d11_present.exe d3d11_present.c -ld3d11 -ldxgi -lgdi32 -luuid -ldxguid
 */
#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(h, m, w, l);
}

int main(int argc, char **argv)
{
    int frames = argc > 1 ? atoi(argv[1]) : 600;
    const char *title = argc > 2 ? argv[2] : "quiver-sight-d3d11";
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"QuiverSightD3D11";
    RegisterClassW(&wc);
    RECT r = {0, 0, 1280, 720};
    AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExA(0, "QuiverSightD3D11", title, WS_OVERLAPPEDWINDOW | WS_VISIBLE, CW_USEDEFAULT, CW_USEDEFAULT,
                                r.right - r.left, r.bottom - r.top, NULL, NULL, wc.hInstance, NULL);
    if (!hwnd) {
        fprintf(stderr, "CreateWindow failed\n");
        return 1;
    }

    DXGI_SWAP_CHAIN_DESC sd = {0};
    sd.BufferCount = 2;
    sd.BufferDesc.Width = 1280;
    sd.BufferDesc.Height = 720;
    sd.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = hwnd;
    sd.SampleDesc.Count = 1;
    sd.Windowed = TRUE;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    ID3D11Device *dev = NULL;
    ID3D11DeviceContext *ctx = NULL;
    IDXGISwapChain *sc = NULL;
    HRESULT hr = D3D11CreateDeviceAndSwapChain(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &sd, &sc, &dev, NULL, &ctx);
    if (FAILED(hr)) {
        fprintf(stderr, "D3D11CreateDeviceAndSwapChain failed: 0x%08lx\n", hr);
        return 2;
    }
    ID3D11Texture2D *bb = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    IDXGISwapChain_GetBuffer(sc, 0, &IID_ID3D11Texture2D, (void **)&bb);
    ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)bb, NULL, &rtv);
    ID3D11Texture2D_Release(bb);

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
        float t = (float)i / (float)frames;
        float color[4] = {0.25f + 0.2f * t, 0.35f, 0.45f - 0.2f * t, 1.0f};
        ID3D11DeviceContext_OMSetRenderTargets(ctx, 1, &rtv, NULL);
        ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, color);
        IDXGISwapChain_Present(sc, 0, 0);
        /* ~120 fps with a hitch now and then, for the graph. */
        Sleep(i % 150 == 75 ? 40 : 8);
    }
    printf("presented %d frames\n", frames);
    ID3D11RenderTargetView_Release(rtv);
    IDXGISwapChain_Release(sc);
    ID3D11DeviceContext_Release(ctx);
    ID3D11Device_Release(dev);
    DestroyWindow(hwnd);
    return 0;
}
