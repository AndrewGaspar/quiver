/* quiver-pad-probe.exe: what a Windows game in a lab instance sees of its game controllers.
 *
 * Run by `quiver lab pad N status` with the instance's own Wine, in the running game's
 * wineserver session, so it sees the devices the game sees. Prints one JSON object:
 *
 *   xinput    XInputGetCapabilities/GetState for slots 0-3 (xinput1_4, else xinput1_3)
 *   dinput    DirectInput 8 game controllers, with their state (DIJOYSTATE2)
 *   hid       HID device interfaces (VID/PID, product string, usage page/usage)
 *   rawinput  raw input HID devices (the names games read with GetRawInputDeviceInfo)
 *
 * `--watch MS N` prints N lines of XInput slot-0 state, one every MS milliseconds (for tests).
 *
 * A GUI-subsystem program that never shows a window: a console one would get Wine's console
 * window, which takes gamescope's focus from the game. Output goes to stdout, which Wine maps
 * to the Unix stdout of the process that started it.
 */
#define WIN32_LEAN_AND_MEAN
#define DIRECTINPUT_VERSION 0x0800
#define INITGUID
#include <windows.h>
#include <setupapi.h>
#include <hidsdi.h>
#include <hidpi.h>
#include <dinput.h>
#include <xinput.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static FILE *out;

static void json_str(const char *s)
{
    fputc('"', out);
    for (; s && *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\') fprintf(out, "\\%c", c);
        else if (c < 0x20) fprintf(out, "\\u%04x", c);
        else fputc(c, out);
    }
    fputc('"', out);
}

static void json_wstr(const WCHAR *w)
{
    char buf[512];
    if (!w) { json_str(""); return; }
    WideCharToMultiByte(CP_UTF8, 0, w, -1, buf, sizeof buf, NULL, NULL);
    buf[sizeof buf - 1] = 0;
    json_str(buf);
}

typedef DWORD (WINAPI *get_state_fn)(DWORD, XINPUT_STATE *);
typedef DWORD (WINAPI *get_caps_fn)(DWORD, DWORD, XINPUT_CAPABILITIES *);
static get_state_fn x_get_state;
static get_caps_fn x_get_caps;
static const char *x_dll = "";

static void load_xinput(void)
{
    static const char *dlls[] = { "xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll" };
    for (unsigned i = 0; i < sizeof dlls / sizeof *dlls; i++) {
        HMODULE h = LoadLibraryA(dlls[i]);
        if (!h) continue;
        x_get_state = (get_state_fn)(void *)GetProcAddress(h, "XInputGetState");
        x_get_caps = (get_caps_fn)(void *)GetProcAddress(h, "XInputGetCapabilities");
        if (x_get_state) { x_dll = dlls[i]; return; }
    }
}

static void xinput(void)
{
    fprintf(out, "\"xinput_dll\":");
    json_str(x_dll);
    fprintf(out, ",\"xinput\":[");
    for (DWORD i = 0; i < 4 && x_get_state; i++) {
        XINPUT_STATE s;
        XINPUT_CAPABILITIES c;
        memset(&s, 0, sizeof s);
        memset(&c, 0, sizeof c);
        DWORD r = x_get_state(i, &s);
        DWORD rc = x_get_caps ? x_get_caps(i, 0, &c) : ERROR_DEVICE_NOT_CONNECTED;
        fprintf(out, "%s{\"slot\":%lu,\"connected\":%s", i ? "," : "", (unsigned long)i, r == ERROR_SUCCESS ? "true" : "false");
        if (r == ERROR_SUCCESS) {
            fprintf(out, ",\"packet\":%lu,\"buttons\":%u,\"lt\":%u,\"rt\":%u,\"lx\":%d,\"ly\":%d,\"rx\":%d,\"ry\":%d",
                    (unsigned long)s.dwPacketNumber, s.Gamepad.wButtons, s.Gamepad.bLeftTrigger, s.Gamepad.bRightTrigger,
                    s.Gamepad.sThumbLX, s.Gamepad.sThumbLY, s.Gamepad.sThumbRX, s.Gamepad.sThumbRY);
        }
        if (rc == ERROR_SUCCESS)
            fprintf(out, ",\"type\":%u,\"subtype\":%u,\"flags\":%u", c.Type, c.SubType, c.Flags);
        fputc('}', out);
    }
    fputc(']', out);
}

static IDirectInput8A *di;
static HWND hwnd;
static int di_count;

static BOOL CALLBACK di_device(const DIDEVICEINSTANCEA *inst, void *ctx)
{
    (void)ctx;
    IDirectInputDevice8A *dev = NULL;
    fprintf(out, "%s{\"name\":", di_count++ ? "," : "");
    json_str(inst->tszInstanceName);
    fprintf(out, ",\"product\":");
    json_str(inst->tszProductName);
    /* guidProduct's first DWORD is PID<<16 | VID for HID devices. */
    fprintf(out, ",\"vid\":%lu,\"pid\":%lu,\"type\":%lu", (unsigned long)(inst->guidProduct.Data1 & 0xffff),
            (unsigned long)(inst->guidProduct.Data1 >> 16), (unsigned long)inst->dwDevType);
    if (SUCCEEDED(IDirectInput8_CreateDevice(di, &inst->guidInstance, &dev, NULL))) {
        DIJOYSTATE2 js;
        DIDEVCAPS caps;
        caps.dwSize = sizeof caps;
        if (SUCCEEDED(IDirectInputDevice8_GetCapabilities(dev, &caps)))
            fprintf(out, ",\"axes\":%lu,\"buttons\":%lu,\"povs\":%lu", (unsigned long)caps.dwAxes, (unsigned long)caps.dwButtons, (unsigned long)caps.dwPOVs);
        if (SUCCEEDED(IDirectInputDevice8_SetDataFormat(dev, &c_dfDIJoystick2))
            && SUCCEEDED(IDirectInputDevice8_SetCooperativeLevel(dev, hwnd, DISCL_BACKGROUND | DISCL_NONEXCLUSIVE))
            && SUCCEEDED(IDirectInputDevice8_Acquire(dev))) {
            memset(&js, 0, sizeof js);
            IDirectInputDevice8_Poll(dev);
            if (SUCCEEDED(IDirectInputDevice8_GetDeviceState(dev, sizeof js, &js))) {
                unsigned pressed = 0;
                for (int b = 0; b < 32; b++)
                    if (js.rgbButtons[b] & 0x80) pressed |= 1u << b;
                fprintf(out, ",\"state\":{\"x\":%ld,\"y\":%ld,\"z\":%ld,\"rx\":%ld,\"ry\":%ld,\"rz\":%ld,\"pov\":%ld,\"pressed\":%u}",
                        js.lX, js.lY, js.lZ, js.lRx, js.lRy, js.lRz, (long)(int)js.rgdwPOV[0], pressed);
            }
            IDirectInputDevice8_Unacquire(dev);
        }
        IDirectInputDevice8_Release(dev);
    }
    fputc('}', out);
    return DIENUM_CONTINUE;
}

static void dinput(void)
{
    fprintf(out, ",\"dinput\":[");
    if (SUCCEEDED(DirectInput8Create(GetModuleHandleA(NULL), DIRECTINPUT_VERSION, &IID_IDirectInput8A, (void **)&di, NULL))) {
        IDirectInput8_EnumDevices(di, DI8DEVCLASS_GAMECTRL, di_device, NULL, DIEDFL_ATTACHEDONLY);
        IDirectInput8_Release(di);
    }
    fputc(']', out);
}

static void hid(void)
{
    GUID g;
    HidD_GetHidGuid(&g);
    HDEVINFO set = SetupDiGetClassDevsA(&g, NULL, NULL, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
    fprintf(out, ",\"hid\":[");
    int n = 0;
    SP_DEVICE_INTERFACE_DATA ifd;
    ifd.cbSize = sizeof ifd;
    for (DWORD i = 0; set != INVALID_HANDLE_VALUE && SetupDiEnumDeviceInterfaces(set, NULL, &g, i, &ifd); i++) {
        char buf[1024];
        SP_DEVICE_INTERFACE_DETAIL_DATA_A *d = (void *)buf;
        d->cbSize = sizeof *d;
        if (!SetupDiGetDeviceInterfaceDetailA(set, &ifd, d, sizeof buf, NULL, NULL)) continue;
        fprintf(out, "%s{\"path\":", n++ ? "," : "");
        json_str(d->DevicePath);
        HANDLE h = CreateFileA(d->DevicePath, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
        if (h != INVALID_HANDLE_VALUE) {
            HIDD_ATTRIBUTES a;
            WCHAR prod[128];
            PHIDP_PREPARSED_DATA pp;
            a.Size = sizeof a;
            if (HidD_GetAttributes(h, &a)) fprintf(out, ",\"vid\":%u,\"pid\":%u,\"version\":%u", a.VendorID, a.ProductID, a.VersionNumber);
            if (HidD_GetProductString(h, prod, sizeof prod)) {
                fprintf(out, ",\"product\":");
                json_wstr(prod);
            }
            if (HidD_GetPreparsedData(h, &pp)) {
                HIDP_CAPS caps;
                if (HidP_GetCaps(pp, &caps) == HIDP_STATUS_SUCCESS) fprintf(out, ",\"usage_page\":%u,\"usage\":%u", caps.UsagePage, caps.Usage);
                HidD_FreePreparsedData(pp);
            }
            CloseHandle(h);
        }
        fputc('}', out);
    }
    if (set != INVALID_HANDLE_VALUE) SetupDiDestroyDeviceInfoList(set);
    fputc(']', out);
}

static void rawinput(void)
{
    UINT n = 0;
    fprintf(out, ",\"rawinput\":[");
    GetRawInputDeviceList(NULL, &n, sizeof(RAWINPUTDEVICELIST));
    RAWINPUTDEVICELIST *l = calloc(n ? n : 1, sizeof *l);
    int k = 0;
    if (n && GetRawInputDeviceList(l, &n, sizeof *l) != (UINT)-1) {
        for (UINT i = 0; i < n; i++) {
            if (l[i].dwType != RIM_TYPEHID) continue;
            char name[512];
            UINT sz = sizeof name;
            RID_DEVICE_INFO info;
            UINT isz = sizeof info;
            info.cbSize = sizeof info;
            if (GetRawInputDeviceInfoA(l[i].hDevice, RIDI_DEVICENAME, name, &sz) == (UINT)-1) name[0] = 0;
            fprintf(out, "%s{\"name\":", k++ ? "," : "");
            json_str(name);
            if (GetRawInputDeviceInfoA(l[i].hDevice, RIDI_DEVICEINFO, &info, &isz) != (UINT)-1)
                fprintf(out, ",\"vid\":%lu,\"pid\":%lu,\"usage_page\":%u,\"usage\":%u", (unsigned long)info.hid.dwVendorId,
                        (unsigned long)info.hid.dwProductId, info.hid.usUsagePage, info.hid.usUsage);
            fputc('}', out);
        }
    }
    free(l);
    fputc(']', out);
}

int WINAPI WinMain(HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show)
{
    (void)inst; (void)prev; (void)show;
    out = stdout;
    load_xinput();
    int ms = 0, count = 0;
    if (sscanf(cmd ? cmd : "", "--watch %d %d", &ms, &count) == 2 && x_get_state) {
        for (int i = 0; i < count; i++) {
            XINPUT_STATE s;
            memset(&s, 0, sizeof s);
            DWORD r = x_get_state(0, &s);
            fprintf(out, "{\"t\":%lu,\"connected\":%s,\"packet\":%lu,\"buttons\":%u,\"lt\":%u,\"rt\":%u,\"lx\":%d,\"ly\":%d,\"rx\":%d,\"ry\":%d}\n",
                    (unsigned long)GetTickCount(), r == ERROR_SUCCESS ? "true" : "false", (unsigned long)s.dwPacketNumber, s.Gamepad.wButtons,
                    s.Gamepad.bLeftTrigger, s.Gamepad.bRightTrigger, s.Gamepad.sThumbLX, s.Gamepad.sThumbLY, s.Gamepad.sThumbRX, s.Gamepad.sThumbRY);
            fflush(out);
            Sleep(ms);
        }
        return 0;
    }
    /* A message-only window for DirectInput's cooperative level: never shown, never focused. */
    hwnd = CreateWindowExA(0, "STATIC", "quiver-pad-probe", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, NULL, NULL);
    fputc('{', out);
    xinput();
    dinput();
    hid();
    rawinput();
    fprintf(out, "}\n");
    fflush(out);
    return 0;
}
