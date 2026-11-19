/* A Windows program that reports every keyboard and mouse message it gets, for
 * tests/wine_input.rs: a borderless window at the screen's top-left, raw input registered for
 * the mouse and keyboard, and one line per message on stdout (flushed):
 *
 *   READY <hwnd>
 *   KEYDOWN <vk> <scan> | KEYUP <vk> <scan>          (WM_KEY*, WM_SYSKEY*)
 *   BUTTON <L|R|M|X> <DOWN|UP> <x> <y>                (WM_*BUTTON*)
 *   MOVE <x> <y>                                      (WM_MOUSEMOVE)
 *   WHEEL <delta>                                     (WM_MOUSEWHEEL)
 *   RAWMOUSE <dx> <dy> <buttonflags>                  (WM_INPUT, mouse)
 *   RAWKEY <vk> <UP|DOWN>                             (WM_INPUT, keyboard)
 *   HELD <vk> <vk> ...                                (every 250 ms: GetAsyncKeyState, 0x01-0xFE)
 *   ACTIVE <0|1>                                      (WM_ACTIVATEAPP)
 *   BYE
 *
 *   input_probe.exe <seconds> <title> [flags]
 *
 * flags: 1 = ClipCursor to the window and hide the cursor (as games do in mouse-look; Wine's
 * X11 driver then grabs the pointer and selects XInput2 raw motion), 2 = SetCursorPos to the
 * window's centre every 50 ms (a game re-centring the pointer).
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -o input_probe.exe input_probe.c -luser32
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static void out(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("\n");
    fflush(stdout);
}

static const char *button(UINT m, WPARAM w)
{
    switch (m) {
    case WM_LBUTTONDOWN: case WM_LBUTTONUP: return "L";
    case WM_RBUTTONDOWN: case WM_RBUTTONUP: return "R";
    case WM_MBUTTONDOWN: case WM_MBUTTONUP: return "M";
    default: return HIWORD(w) == XBUTTON1 ? "X1" : "X2";
    }
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    switch (m) {
    case WM_KEYDOWN: case WM_SYSKEYDOWN:
        out("KEYDOWN 0x%02x 0x%02x", (unsigned)w, (unsigned)((l >> 16) & 0x1ff));
        return 0;
    case WM_KEYUP: case WM_SYSKEYUP:
        out("KEYUP 0x%02x 0x%02x", (unsigned)w, (unsigned)((l >> 16) & 0x1ff));
        return 0;
    case WM_LBUTTONDOWN: case WM_RBUTTONDOWN: case WM_MBUTTONDOWN: case WM_XBUTTONDOWN:
        out("BUTTON %s DOWN %d %d", button(m, w), (short)LOWORD(l), (short)HIWORD(l));
        return 0;
    case WM_LBUTTONUP: case WM_RBUTTONUP: case WM_MBUTTONUP: case WM_XBUTTONUP:
        out("BUTTON %s UP %d %d", button(m, w), (short)LOWORD(l), (short)HIWORD(l));
        return 0;
    case WM_MOUSEMOVE:
        out("MOVE %d %d", (short)LOWORD(l), (short)HIWORD(l));
        return 0;
    case WM_MOUSEWHEEL:
        out("WHEEL %d", (short)HIWORD(w));
        return 0;
    case WM_ACTIVATEAPP:
        out("ACTIVE %d", w ? 1 : 0);
        break;
    case WM_INPUT: {
        RAWINPUT ri;
        UINT size = sizeof(ri);
        if (GetRawInputData((HRAWINPUT)l, RID_INPUT, &ri, &size, sizeof(RAWINPUTHEADER)) != (UINT)-1) {
            if (ri.header.dwType == RIM_TYPEMOUSE)
                out("RAWMOUSE %ld %ld 0x%x", ri.data.mouse.lLastX, ri.data.mouse.lLastY, ri.data.mouse.usButtonFlags);
            else if (ri.header.dwType == RIM_TYPEKEYBOARD)
                out("RAWKEY 0x%02x %s", ri.data.keyboard.VKey, (ri.data.keyboard.Flags & RI_KEY_BREAK) ? "UP" : "DOWN");
        }
        break;
    }
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(h, m, w, l);
}

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 30;
    const char *title = argc > 2 ? argv[2] : "quiver-input-probe";
    int flags = argc > 3 ? atoi(argv[3]) : 0;
    int w = GetSystemMetrics(SM_CXSCREEN), hgt = GetSystemMetrics(SM_CYSCREEN);
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"QuiverInputProbe";
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    HWND hwnd = CreateWindowExA(0, "QuiverInputProbe", title, WS_POPUP | WS_VISIBLE, 0, 0, w, hgt, NULL, NULL, wc.hInstance, NULL);
    if (!hwnd) {
        fprintf(stderr, "CreateWindow failed\n");
        return 1;
    }
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    RAWINPUTDEVICE rid[2] = {{1, 2, 0, hwnd}, {1, 6, 0, hwnd}};
    if (!RegisterRawInputDevices(rid, 2, sizeof(rid[0])))
        fprintf(stderr, "RegisterRawInputDevices failed: %lu\n", GetLastError());
    if (flags & 1) {
        RECT r;
        GetWindowRect(hwnd, &r);
        ClipCursor(&r);
        ShowCursor(FALSE);
    }
    out("READY %p", (void *)hwnd);
    DWORD start = GetTickCount(), last_held = 0, last_warp = 0;
    MSG msg;
    while (GetTickCount() - start < (DWORD)seconds * 1000) {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT)
                goto done;
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DWORD now = GetTickCount();
        if (now - last_held >= 250) {
            char line[1024] = "HELD";
            for (int vk = 1; vk < 0xff; vk++)
                if (GetAsyncKeyState(vk) & 0x8000)
                    sprintf(line + strlen(line), " 0x%02x", vk);
            out("%s", line);
            last_held = now;
        }
        if ((flags & 2) && now - last_warp >= 50) {
            SetCursorPos(w / 2, hgt / 2);
            last_warp = now;
        }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 10, QS_ALLINPUT);
    }
done:
    out("BYE");
    DestroyWindow(hwnd);
    return 0;
}
