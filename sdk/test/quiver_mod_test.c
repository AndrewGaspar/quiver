/* A stand-in mod for testing quiver_mod.h against quiver-bridge under Wine
 * (crates/quiver-core/tests/modsdk_wine.rs). Built as a console exe so it can print what it
 * sees; a real mod is a DLL, which changes nothing about the SDK.
 *
 *   quiver_mod_test.exe [seconds] [poll]
 *
 * Surface "quiver-test". Applies options test.mode (any) and test.level (0-10, else
 * rejected) live; declares gauge, counter, state, text and event metrics plus a frame
 * metric Quiver refuses (no rings yet); reports a change of its own and a log line.
 * Prints one line per thing that happened, for the test to check. */
#define QUIVER_MOD_VERSION "test-1"
#define QUIVER_MOD_IMPLEMENTATION
#include "quiver_mod.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static quiver_mod *Q;

static int on_values(void *user, const quiver_value *v, int n) {
    int i;
    (void)user;
    for (i = 0; i < n; i++) {
        printf("value %s=%s json=%s\n", v[i].id, v[i].text, v[i].json);
        if (strcmp(v[i].id, "test.level") == 0 && atoi(v[i].text) > 10) quiver_reject(Q, v[i].id, "level is at most 10");
    }
    fflush(stdout);
    return 0;
}

static void on_action(void *user, const char *id, const char *arg) {
    (void)user;
    printf("action %s arg=%s\n", id, arg ? arg : "(none)");
    fflush(stdout);
}

int main(int argc, char **argv) {
    int seconds = argc > 1 ? atoi(argv[1]) : 10;
    unsigned flags = argc > 2 && strcmp(argv[2], "poll") == 0 ? QUIVER_POLL : 0;
    quiver_metric fps, frames, fg, ups, bench, frame;
    quiver_stats st;
    int i, was = 0;
    char buf[64];

    setvbuf(stdout, NULL, _IONBF, 0);
    Q = quiver_open("quiver-test", NULL, flags);
    quiver_on_values_cb(Q, on_values, NULL);
    quiver_on_action_cb(Q, on_action, NULL);
    quiver_handle_option(Q, "test.mode");
    quiver_handle_option(Q, "test.level");
    fps = quiver_metric_declare(Q, "fps.base", QUIVER_GAUGE, "{\"unit\":\"fps\",\"label\":\"Base FPS\"}");
    frames = quiver_metric_declare(Q, "frames.real", QUIVER_COUNTER, NULL);
    fg = quiver_metric_declare(Q, "fg.state", QUIVER_STATE,
                               "{\"label\":\"Frame Generation\",\"short\":\"FG\",\"values\":[{\"value\":\"off\",\"level\":\"dim\"},{\"value\":\"on\"}]}");
    ups = quiver_metric_declare(Q, "upscaler", QUIVER_TEXT, NULL);
    bench = quiver_metric_declare(Q, "bench", QUIVER_EVENT, "{}");
    frame = quiver_metric_declare(Q, "frame", QUIVER_FRAME, NULL);
    printf("open %s declare=%d,%d,%d,%d,%d,%d bad=%d\n", quiver_metric_declare(Q, "x", QUIVER_GAUGE, NULL) < 0 ? "noop" : "live", fps, frames, fg, ups,
           bench, frame, quiver_metric_declare(Q, "Bad Name", QUIVER_GAUGE, NULL));

    for (i = 0; i < seconds * 50; i++) {
        int c = quiver_connected(Q);
        if (c != was) {
            printf("connected=%d\n", c);
            was = c;
        }
        quiver_gauge(Q, fps, 79.5 + (i % 3));
        quiver_count(Q, frames, 1);
        quiver_state(Q, fg, (i / 50) % 2 ? "on" : "off");
        quiver_text(Q, ups, "DLSS K \xff"); /* invalid UTF-8: replaced, not sent raw */
        quiver_frame_submit(Q, frame, NULL);
        if (i == 50) {
            quiver_changed(Q, "test.mode", "fast", 0);
            quiver_readout(Q, "res", "1280x800");
            quiver_mark(Q, bench, quiver_now(), 0, "begin");
            quiver_log(Q, QUIVER_LOG_INFO, "test mod says hi");
            printf("metric status fps=%d frame=%d\n", quiver_metric_status(Q, fps), quiver_metric_status(Q, frame));
        }
        if (flags & QUIVER_POLL) quiver_poll(Q);
        Sleep(20);
    }
    if (quiver_get(Q, "test.mode", buf, sizeof buf)) printf("get test.mode=%s\n", buf);
    quiver_get_stats(Q, &st);
    printf("stats connects=%lu sent=%lu received=%lu dropped=%lu\n", st.connects, st.sent, st.received, st.dropped);
    quiver_close(Q);
    printf("closed\n");
    return 0;
}
