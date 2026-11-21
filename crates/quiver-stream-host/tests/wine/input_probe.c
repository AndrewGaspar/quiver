/* A Windows program for quiver-stream-host's live tests: a full-screen window that logs the
 * input Wine gives it, one line each, to the file named on its command line:
 *   ready
 *   keydown <vk> / keyup <vk>
 *   raw <dx> <dy>          (WM_INPUT relative mouse motion: Wine's raw input, from XI2)
 *   lbutton down / lbutton up
 *   wheel <delta>
 * Build: x86_64-w64-mingw32-gcc -mwindows -O2 input_probe.c -o input_probe.exe
 */
#include <windows.h>
#include <stdio.h>

static FILE *out;

static void say(const char *fmt, long a, long b)
{
    fprintf(out, fmt, a, b);
    fputc('\n', out);
    fflush(out);
}

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    switch (m) {
    case WM_INPUT: {
        RAWINPUT ri;
        UINT size = sizeof(ri);
        if (GetRawInputData((HRAWINPUT)l, RID_INPUT, &ri, &size, sizeof(RAWINPUTHEADER)) != (UINT)-1
            && ri.header.dwType == RIM_TYPEMOUSE && !(ri.data.mouse.usFlags & MOUSE_MOVE_ABSOLUTE)
            && (ri.data.mouse.lLastX || ri.data.mouse.lLastY))
            say("raw %ld %ld", ri.data.mouse.lLastX, ri.data.mouse.lLastY);
        break;
    }
    case WM_KEYDOWN: say("keydown %ld", (long)w, 0); break;
    case WM_KEYUP: say("keyup %ld", (long)w, 0); break;
    case WM_LBUTTONDOWN: say("lbutton down", 0, 0); break;
    case WM_LBUTTONUP: say("lbutton up", 0, 0); break;
    case WM_MOUSEWHEEL: say("wheel %ld", (long)GET_WHEEL_DELTA_WPARAM(w), 0); break;
    case WM_DESTROY: PostQuitMessage(0); return 0;
    }
    return DefWindowProcA(h, m, w, l);
}

int WINAPI WinMain(HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show)
{
    (void)prev;
    (void)show;
    out = fopen(cmd, "a");
    if (!out)
        return 1;
    WNDCLASSA wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = inst;
    wc.hCursor = LoadCursor(NULL, IDC_ARROW);
    wc.hbrBackground = (HBRUSH)GetStockObject(GRAY_BRUSH);
    wc.lpszClassName = "quiver_stream_probe";
    RegisterClassA(&wc);
    HWND hwnd = CreateWindowExA(0, wc.lpszClassName, "quiver-stream input probe", WS_POPUP | WS_VISIBLE, 0, 0,
                                GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), NULL, NULL, inst, NULL);
    RAWINPUTDEVICE rid = {1, 2, RIDEV_INPUTSINK, hwnd};
    RegisterRawInputDevices(&rid, 1, sizeof(rid));
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    say("ready", 0, 0);
    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0) > 0) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }
    return 0;
}
