/* A fake Wayland compositor for tests/wayland_input.rs: just enough for Wine's Wayland
 * driver (winewayland) to map a window and get keyboard and pointer input, which the test
 * types in through stdin. Never draws anything; releases every buffer at once.
 *
 *   fakecomp <socket name>        (in $XDG_RUNTIME_DIR, a private directory the test makes)
 *
 * Globals: wl_compositor 6, wl_subcompositor 1, wl_shm 1, wl_seat 9 (pointer + keyboard),
 * wl_output 4 (1024x768), xdg_wm_base 6, wp_viewporter 1, zwp_relative_pointer_manager_v1 1,
 * zwp_pointer_constraints_v1 1. Requests nobody here cares about get a resource (new_id) or
 * are ignored, through one generic dispatcher.
 *
 * stdin, one command per line:
 *   focus                    keyboard + pointer enter on the last mapped toplevel
 *   key <evdev code> <0|1>   wl_keyboard.key to the focused client's keyboards
 *   button <code> <0|1>      wl_pointer.button + frame
 *   motion <x> <y>           wl_pointer.motion + frame
 *   rel <dx> <dy>            zwp_relative_pointer_v1.relative_motion
 *   axis <discrete>          wl_pointer.axis_discrete + axis (vertical) + frame
 * stdout: READY, MAPPED <n>, KEYBOARD, POINTER, RELATIVE, FOCUSED, and NOTE lines.
 *
 * Build (the test does it): wayland-scanner server-header / private-code for the protocols,
 * then cc fakecomp.c *.c $(pkg-config --cflags --libs wayland-server xkbcommon)
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <time.h>
#include <wayland-server.h>
#include <xkbcommon/xkbcommon.h>
#include "xdg-shell-server-protocol.h"
#include "viewporter-server-protocol.h"
#include "relative-pointer-unstable-v1-server-protocol.h"
#include "pointer-constraints-unstable-v1-server-protocol.h"

static struct wl_display *display;
static uint32_t serial_counter = 1;

static uint32_t next_serial(void) { return serial_counter++; }

static uint32_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

static void out(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("\n");
    fflush(stdout);
}

/* Resource lists we send to. */
#define MAX_RES 64
struct list { struct wl_resource *r[MAX_RES]; int n; };
static struct list keyboards, pointers, relatives;

static void list_add(struct list *l, struct wl_resource *r) { if (l->n < MAX_RES) l->r[l->n++] = r; }
static void list_del(struct list *l, struct wl_resource *r)
{
    for (int i = 0; i < l->n; i++)
        if (l->r[i] == r) { l->r[i] = l->r[--l->n]; return; }
}

/* Surfaces: the buffer attached and frame callbacks pending, released at commit. */
struct surface {
    struct wl_resource *res;
    struct wl_resource *buffer;
    struct wl_resource *frames[16];
    int nframes;
    int toplevel; /* has a configured xdg_toplevel */
    int mapped;
};

static struct wl_resource *focus_surface; /* last mapped toplevel's wl_surface */
static int mapped_count;

static int dispatch(const void *impl, void *target, uint32_t opcode, const struct wl_message *msg, union wl_argument *args);

static void destroy_listed(struct wl_resource *r)
{
    list_del(&keyboards, r);
    list_del(&pointers, r);
    list_del(&relatives, r);
}

static void surface_destroyed(struct wl_resource *r)
{
    struct surface *s = wl_resource_get_user_data(r);
    if (focus_surface == r) focus_surface = NULL;
    free(s);
}

static struct wl_resource *make(struct wl_client *c, const struct wl_interface *iface, int version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, iface, version, id);
    if (!r) { wl_client_post_no_memory(c); return NULL; }
    wl_resource_set_dispatcher(r, dispatch, NULL, NULL, destroy_listed);
    return r;
}

static void send_keymap(struct wl_resource *kb)
{
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_rule_names names = { .rules = "evdev", .model = "pc105", .layout = "us" };
    struct xkb_keymap *km = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    char *str = km ? xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1) : NULL;
    if (str) {
        size_t len = strlen(str) + 1;
        int fd = memfd_create("fakecomp-keymap", MFD_CLOEXEC);
        if (fd >= 0 && write(fd, str, len) == (ssize_t)len) {
            wl_keyboard_send_keymap(kb, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, (uint32_t)len);
        }
        if (fd >= 0) close(fd);
        free(str);
    } else {
        out("NOTE no keymap");
    }
    if (km) xkb_keymap_unref(km);
    xkb_context_unref(ctx);
    if (wl_resource_get_version(kb) >= 4) wl_keyboard_send_repeat_info(kb, 25, 600);
}

static void enter(struct wl_resource *kb_or_ptr, int is_kb)
{
    if (!focus_surface || wl_resource_get_client(kb_or_ptr) != wl_resource_get_client(focus_surface)) return;
    if (is_kb) {
        struct wl_array keys;
        wl_array_init(&keys);
        wl_keyboard_send_enter(kb_or_ptr, next_serial(), focus_surface, &keys);
        wl_array_release(&keys);
        wl_keyboard_send_modifiers(kb_or_ptr, next_serial(), 0, 0, 0, 0);
    } else {
        wl_pointer_send_enter(kb_or_ptr, next_serial(), focus_surface, wl_fixed_from_int(100), wl_fixed_from_int(100));
        if (wl_resource_get_version(kb_or_ptr) >= 5) wl_pointer_send_frame(kb_or_ptr);
    }
}

static int same_client(struct wl_resource *r)
{
    return focus_surface && wl_resource_get_client(r) == wl_resource_get_client(focus_surface);
}

/* The generic request handler: per interface and request name. */
static int dispatch(const void *impl, void *target, uint32_t opcode, const struct wl_message *msg, union wl_argument *args)
{
    struct wl_resource *res = target;
    struct wl_client *c = wl_resource_get_client(res);
    const char *iface = wl_resource_get_class(res);
    const char *req = msg->name;
    int version = wl_resource_get_version(res);
    (void)impl;
    (void)opcode;

    if (!strcmp(iface, "wl_compositor") && !strcmp(req, "create_surface")) {
        struct wl_resource *r = wl_resource_create(c, &wl_surface_interface, version, args[0].n);
        struct surface *s = calloc(1, sizeof(*s));
        s->res = r;
        wl_resource_set_dispatcher(r, dispatch, NULL, s, surface_destroyed);
        return 0;
    }
    if (!strcmp(iface, "wl_surface")) {
        struct surface *s = wl_resource_get_user_data(res);
        if (!strcmp(req, "destroy")) { wl_resource_destroy(res); return 0; }
        if (!strcmp(req, "attach")) { s->buffer = args[0].o ? (struct wl_resource *)args[0].o : NULL; return 0; }
        if (!strcmp(req, "frame")) {
            struct wl_resource *cb = wl_resource_create(c, &wl_callback_interface, 1, args[0].n);
            if (s->nframes < 16) s->frames[s->nframes++] = cb;
            return 0;
        }
        if (!strcmp(req, "commit")) {
            if (s->buffer) {
                wl_buffer_send_release(s->buffer);
                if (s->toplevel && !s->mapped) {
                    s->mapped = 1;
                    focus_surface = res;
                    out("MAPPED %d", ++mapped_count);
                }
                s->buffer = NULL;
            }
            for (int i = 0; i < s->nframes; i++) {
                wl_callback_send_done(s->frames[i], now_ms());
                wl_resource_destroy(s->frames[i]);
            }
            s->nframes = 0;
            return 0;
        }
        /* damage, set_opaque_region, set_input_region, set_buffer_scale, offset, ...: no-ops */
        return 0;
    }
    if (!strcmp(iface, "wl_shm") && !strcmp(req, "create_pool")) {
        close(args[1].h);
        make(c, &wl_shm_pool_interface, version, args[0].n);
        return 0;
    }
    if (!strcmp(iface, "xdg_wm_base") && !strcmp(req, "get_xdg_surface")) {
        struct wl_resource *r = make(c, &xdg_surface_interface, version, args[0].n);
        wl_resource_set_user_data(r, args[1].o);
        return 0;
    }
    if (!strcmp(iface, "xdg_surface") && !strcmp(req, "get_toplevel")) {
        struct wl_resource *surf = wl_resource_get_user_data(res);
        struct wl_resource *t = make(c, &xdg_toplevel_interface, version, args[0].n);
        struct wl_array states;
        wl_array_init(&states);
        uint32_t *st = wl_array_add(&states, sizeof(uint32_t));
        *st = XDG_TOPLEVEL_STATE_ACTIVATED;
        xdg_toplevel_send_configure(t, 0, 0, &states);
        wl_array_release(&states);
        xdg_surface_send_configure(res, next_serial());
        if (surf) ((struct surface *)wl_resource_get_user_data(surf))->toplevel = 1;
        return 0;
    }
    if (!strcmp(iface, "wl_seat")) {
        if (!strcmp(req, "get_keyboard")) {
            struct wl_resource *kb = make(c, &wl_keyboard_interface, version, args[0].n);
            list_add(&keyboards, kb);
            send_keymap(kb);
            out("KEYBOARD");
            enter(kb, 1);
            return 0;
        }
        if (!strcmp(req, "get_pointer")) {
            struct wl_resource *p = make(c, &wl_pointer_interface, version, args[0].n);
            list_add(&pointers, p);
            out("POINTER");
            enter(p, 0);
            return 0;
        }
    }
    if (!strcmp(iface, "zwp_relative_pointer_manager_v1") && !strcmp(req, "get_relative_pointer")) {
        list_add(&relatives, make(c, &zwp_relative_pointer_v1_interface, version, args[0].n));
        out("RELATIVE");
        return 0;
    }
    if (!strcmp(iface, "zwp_pointer_constraints_v1") && (!strcmp(req, "lock_pointer") || !strcmp(req, "confine_pointer"))) {
        struct wl_resource *r = make(c, !strcmp(req, "lock_pointer") ? &zwp_locked_pointer_v1_interface : &zwp_confined_pointer_v1_interface, version, args[0].n);
        if (!strcmp(req, "lock_pointer")) zwp_locked_pointer_v1_send_locked(r);
        else zwp_confined_pointer_v1_send_confined(r);
        out("NOTE %s", req);
        return 0;
    }
    /* Destructors. */
    if (!strcmp(req, "destroy") || !strcmp(req, "release")) {
        wl_resource_destroy(res);
        return 0;
    }
    /* Anything that makes an object: a resource of the type the message says. */
    for (int i = 0, a = 0; msg->signature[i]; i++) {
        char t = msg->signature[i];
        if (t >= '0' && t <= '9') continue;
        if (t == '?') continue;
        if (t == 'n' && msg->types[a]) make(c, msg->types[a], version, args[a].n);
        if (t == 'h') close(args[a].h);
        a++;
    }
    return 0;
}

static void bind_generic(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    const struct wl_interface *iface = data;
    struct wl_resource *r = make(c, iface, (int)version, id);
    if (!r) return;
    if (iface == &wl_seat_interface) {
        wl_seat_send_capabilities(r, WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD);
        if (version >= 2) wl_seat_send_name(r, "seat0");
    } else if (iface == &wl_shm_interface) {
        wl_shm_send_format(r, WL_SHM_FORMAT_ARGB8888);
        wl_shm_send_format(r, WL_SHM_FORMAT_XRGB8888);
    } else if (iface == &wl_output_interface) {
        wl_output_send_geometry(r, 0, 0, 300, 200, WL_OUTPUT_SUBPIXEL_UNKNOWN, "Quiver", "Fake", WL_OUTPUT_TRANSFORM_NORMAL);
        wl_output_send_mode(r, WL_OUTPUT_MODE_CURRENT | WL_OUTPUT_MODE_PREFERRED, 1024, 768, 60000);
        if (version >= 2) wl_output_send_scale(r, 1);
        if (version >= 4) wl_output_send_name(r, "FAKE-1");
        if (version >= 2) wl_output_send_done(r);
    }
}

static void for_focused(struct list *l, void (*f)(struct wl_resource *, void *), void *arg)
{
    for (int i = 0; i < l->n; i++)
        if (same_client(l->r[i])) f(l->r[i], arg);
}

struct two { int a, b; };
static void do_key(struct wl_resource *r, void *p) { struct two *t = p; wl_keyboard_send_key(r, next_serial(), now_ms(), t->a, t->b); }
static void do_button(struct wl_resource *r, void *p)
{
    struct two *t = p;
    wl_pointer_send_button(r, next_serial(), now_ms(), t->a, t->b);
    if (wl_resource_get_version(r) >= 5) wl_pointer_send_frame(r);
}
static void do_motion(struct wl_resource *r, void *p)
{
    struct two *t = p;
    wl_pointer_send_motion(r, now_ms(), wl_fixed_from_int(t->a), wl_fixed_from_int(t->b));
    if (wl_resource_get_version(r) >= 5) wl_pointer_send_frame(r);
}
static void do_rel(struct wl_resource *r, void *p)
{
    struct two *t = p;
    uint64_t us = (uint64_t)now_ms() * 1000;
    zwp_relative_pointer_v1_send_relative_motion(r, us >> 32, (uint32_t)us, wl_fixed_from_int(t->a), wl_fixed_from_int(t->b), wl_fixed_from_int(t->a), wl_fixed_from_int(t->b));
}
static void do_axis(struct wl_resource *r, void *p)
{
    struct two *t = p;
    int v = wl_resource_get_version(r);
    if (v >= 5) wl_pointer_send_axis_source(r, t->b ? WL_POINTER_AXIS_SOURCE_FINGER : WL_POINTER_AXIS_SOURCE_WHEEL);
    if (!t->b && v >= 8) wl_pointer_send_axis_value120(r, WL_POINTER_AXIS_VERTICAL_SCROLL, t->a * 120);
    else if (!t->b && v >= 5) wl_pointer_send_axis_discrete(r, WL_POINTER_AXIS_VERTICAL_SCROLL, t->a);
    wl_pointer_send_axis(r, now_ms(), WL_POINTER_AXIS_VERTICAL_SCROLL, wl_fixed_from_int(t->a * 10));
    if (wl_resource_get_version(r) >= 5) wl_pointer_send_frame(r);
}

static int on_stdin(int fd, uint32_t mask, void *data)
{
    static char buf[4096];
    static size_t have;
    (void)data;
    if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) { wl_display_terminate(display); return 0; }
    ssize_t n = read(fd, buf + have, sizeof(buf) - 1 - have);
    if (n <= 0) { wl_display_terminate(display); return 0; }
    have += (size_t)n;
    buf[have] = 0;
    char *line = buf, *nl;
    while ((nl = strchr(line, '\n'))) {
        *nl = 0;
        struct two t = {0, 0};
        char cmd[32] = {0};
        sscanf(line, "%31s %d %d", cmd, &t.a, &t.b);
        if (!strcmp(cmd, "focus")) {
            for (int i = 0; i < keyboards.n; i++) enter(keyboards.r[i], 1);
            for (int i = 0; i < pointers.n; i++) enter(pointers.r[i], 0);
            out("FOCUSED");
        } else if (!strcmp(cmd, "key")) for_focused(&keyboards, do_key, &t);
        else if (!strcmp(cmd, "button")) for_focused(&pointers, do_button, &t);
        else if (!strcmp(cmd, "motion")) for_focused(&pointers, do_motion, &t);
        else if (!strcmp(cmd, "rel")) for_focused(&relatives, do_rel, &t);
        else if (!strcmp(cmd, "axis")) for_focused(&pointers, do_axis, &t);
        line = nl + 1;
    }
    have = strlen(line);
    memmove(buf, line, have);
    wl_display_flush_clients(display);
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 2) return 2;
    display = wl_display_create();
    if (wl_display_add_socket(display, argv[1]) != 0) { perror("add_socket"); return 1; }
    struct { const struct wl_interface *i; int v; } globals[] = {
        {&wl_compositor_interface, 6}, {&wl_subcompositor_interface, 1}, {&wl_shm_interface, 1},
        {&wl_seat_interface, 9}, {&wl_output_interface, 4}, {&xdg_wm_base_interface, 6},
        {&wp_viewporter_interface, 1}, {&zwp_relative_pointer_manager_v1_interface, 1},
        {&zwp_pointer_constraints_v1_interface, 1},
    };
    for (size_t i = 0; i < sizeof(globals) / sizeof(globals[0]); i++)
        wl_global_create(display, globals[i].i, globals[i].v, (void *)globals[i].i, bind_generic);
    wl_event_loop_add_fd(wl_display_get_event_loop(display), 0, WL_EVENT_READABLE, on_stdin, NULL);
    out("READY");
    wl_display_run(display);
    wl_display_destroy(display);
    return 0;
}
