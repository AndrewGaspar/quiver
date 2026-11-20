/* pad_game.exe: a stand-in game for the lab pad's end-to-end test (crates/quiver-cli/tests/lab_pad.rs).
 *
 * A D3D11 window (through DXVK, as Proton sets up) that, every frame, reads XInput slots 0 and
 * 1 and logs each change with the frame number to QUIVER_PAD_GAME_LOG:
 *
 *   frame 812 slot 0 buttons 0x1000 lt 0 rt 255 lx 0 ly 32767 rx 0 ry 0
 *   frame 812 slot 1 connected | disconnected
 *
 * and clears the window to a colour that shows slot 0's state, so a screenshot shows the game
 * reacting: A green, B red, X blue, Y yellow, the right trigger brightens it, the left stick
 * moves a white block, nothing pressed is dark grey.
 *
 * It imports winmm (timeGetTime, once per frame) so the lab's virtual clock has a way in
 * (the proxy goes in as winmm.dll), and logs the frame's timeGetTime value too.
 */
#define WIN32_LEAN_AND_MEAN
#define COBJMACROS
#include <windows.h>
#include <mmsystem.h>
#include <d3d11_1.h>
#include <xinput.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
    return DefWindowProcA(h, m, w, l);
}

typedef DWORD (WINAPI *get_state_fn)(DWORD, XINPUT_STATE *);

int WINAPI WinMain(HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show)
{
    (void)prev; (void)cmd; (void)show;
    char path[MAX_PATH] = "";
    GetEnvironmentVariableA("QUIVER_PAD_GAME_LOG", path, sizeof path);
    FILE *log = path[0] ? fopen(path, "a") : NULL;
    HMODULE xi = LoadLibraryA("xinput1_4.dll");
    if (!xi) xi = LoadLibraryA("xinput1_3.dll");
    get_state_fn get_state = xi ? (get_state_fn)(void *)GetProcAddress(xi, "XInputGetState") : NULL;
    if (log) { fprintf(log, "start xinput %s\n", get_state ? "ok" : "missing"); fflush(log); }

    WNDCLASSA wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = inst;
    wc.lpszClassName = "quiver_pad_game";
    RegisterClassA(&wc);
    HWND wnd = CreateWindowA(wc.lpszClassName, "quiver pad game", WS_POPUP | WS_VISIBLE, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), NULL, NULL, inst, NULL);
    SetForegroundWindow(wnd);

    DXGI_SWAP_CHAIN_DESC sd;
    memset(&sd, 0, sizeof sd);
    sd.BufferCount = 2;
    sd.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = wnd;
    sd.SampleDesc.Count = 1;
    sd.Windowed = TRUE;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    IDXGISwapChain *sc = NULL;
    ID3D11Device *dev = NULL;
    ID3D11DeviceContext *ctx = NULL;
    if (FAILED(D3D11CreateDeviceAndSwapChain(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &sd, &sc, &dev, NULL, &ctx))) {
        if (log) { fprintf(log, "no d3d11\n"); fclose(log); }
        return 1;
    }
    ID3D11Texture2D *bb = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    IDXGISwapChain_GetBuffer(sc, 0, &IID_ID3D11Texture2D, (void **)&bb);
    ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)bb, NULL, &rtv);
    ID3D11Texture2D_Release(bb);

    XINPUT_STATE last[2];
    BOOL had[2] = { FALSE, FALSE }, seen[2] = { FALSE, FALSE };
    memset(last, 0, sizeof last);
    for (unsigned long frame = 1;; frame++) {
        MSG msg;
        while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) return 0;
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
        DWORD ms = timeGetTime();
        XINPUT_STATE s0;
        memset(&s0, 0, sizeof s0);
        for (DWORD slot = 0; slot < 2 && get_state; slot++) {
            XINPUT_STATE s;
            memset(&s, 0, sizeof s);
            BOOL on = get_state(slot, &s) == ERROR_SUCCESS;
            if (slot == 0) s0 = s;
            if (log && (on != had[slot] || !seen[slot])) {
                fprintf(log, "frame %lu t %lu slot %lu %s\n", frame, (unsigned long)ms, (unsigned long)slot, on ? "connected" : "disconnected");
                had[slot] = on;
                seen[slot] = TRUE;
            }
            if (log && on && memcmp(&s.Gamepad, &last[slot].Gamepad, sizeof s.Gamepad)) {
                fprintf(log, "frame %lu t %lu slot %lu buttons 0x%04x lt %u rt %u lx %d ly %d rx %d ry %d\n", frame, (unsigned long)ms, (unsigned long)slot,
                        s.Gamepad.wButtons, s.Gamepad.bLeftTrigger, s.Gamepad.bRightTrigger, s.Gamepad.sThumbLX, s.Gamepad.sThumbLY, s.Gamepad.sThumbRX, s.Gamepad.sThumbRY);
                fflush(log);
            }
            last[slot] = s;
        }
        WORD b = s0.Gamepad.wButtons;
        float k = 0.35f + 0.65f * s0.Gamepad.bRightTrigger / 255.0f;
        float c[4] = { 0.08f, 0.08f, 0.08f, 1.0f };
        if (b & XINPUT_GAMEPAD_A) { c[0] = 0.0f; c[1] = k; c[2] = 0.0f; }
        if (b & XINPUT_GAMEPAD_B) { c[0] = k; c[1] = 0.0f; c[2] = 0.0f; }
        if (b & XINPUT_GAMEPAD_X) { c[0] = 0.0f; c[1] = 0.0f; c[2] = k; }
        if (b & XINPUT_GAMEPAD_Y) { c[0] = k; c[1] = k; c[2] = 0.0f; }
        ID3D11DeviceContext_OMSetRenderTargets(ctx, 1, &rtv, NULL);
        ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, c);
        /* The left stick moves a white block from the middle (Y up). */
        D3D11_RECT r;
        RECT cr;
        GetClientRect(wnd, &cr);
        int cx = cr.right / 2 + (int)(s0.Gamepad.sThumbLX / 32768.0f * cr.right / 3);
        int cy = cr.bottom / 2 - (int)(s0.Gamepad.sThumbLY / 32768.0f * cr.bottom / 3);
        r.left = cx - 40; r.right = cx + 40; r.top = cy - 40; r.bottom = cy + 40;
        ID3D11DeviceContext1 *ctx1 = NULL;
        if (SUCCEEDED(ID3D11DeviceContext_QueryInterface(ctx, &IID_ID3D11DeviceContext1, (void **)&ctx1))) {
            float white[4] = { 1, 1, 1, 1 };
            ID3D11DeviceContext1_ClearView(ctx1, (ID3D11View *)rtv, white, &r, 1);
            ID3D11DeviceContext1_Release(ctx1);
        }
        IDXGISwapChain_Present(sc, 1, 0);
    }
}
