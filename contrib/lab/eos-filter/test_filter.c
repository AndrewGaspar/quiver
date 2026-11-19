/* Unit tests of the EOS filter's parser and allowlist, built and run natively:
 *   cc -Wall -Wextra -o test_filter test_filter.c filter.c && ./test_filter
 * (crates/quiver-core/tests/eos_filter.rs does that under cargo test). */
#include "filter.h"
#include "allowlist.h"

#include <stdio.h>
#include <string.h>

static int failures, checks;
#define CHECK(cond)                                                                    \
    do {                                                                               \
        checks++;                                                                      \
        if (!(cond)) {                                                                 \
            failures++;                                                                \
            fprintf(stderr, "%s:%d: %s: CHECK(%s) failed\n", __FILE__, __LINE__, __func__, #cond); \
        }                                                                              \
    } while (0)

#define API "Host: api.epicgames.dev\r\n"

/* One write; returns the verdict. */
static int w(ef_conn *c, const char *s) {
    ef_begin(c);
    ef_data(c, (const unsigned char *)s, strlen(s));
    return ef_end(c);
}

static int wn(ef_conn *c, const char *s, size_t n) {
    ef_begin(c);
    ef_data(c, (const unsigned char *)s, n);
    return ef_end(c);
}

/* One write made of several data buffers (as one EncryptMessage with several). */
static int w2(ef_conn *c, const char *a, const char *b) {
    ef_begin(c);
    ef_data(c, (const unsigned char *)a, strlen(a));
    ef_data(c, (const unsigned char *)b, strlen(b));
    return ef_end(c);
}

static int has_event(ef_conn *c, int allowed, const char *method, const char *target) {
    for (int i = 0; i < c->nev; i++)
        if (c->ev[i].allowed == allowed && !strcmp(c->ev[i].method, method) && !strcmp(c->ev[i].target, target)) return 1;
    return 0;
}

static void allowed_and_blocked(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: 9\r\n\r\ngrant=abc") == 1);
    CHECK(c.nev == 1 && has_event(&c, 1, "POST", "/epic/oauth/v1/token"));
    CHECK(!strcmp(c.ev[0].why, "Epic account sign-in (token)"));
    /* keep-alive: the next request on the connection, a cloud save upload */
    CHECK(w(&c, "PUT /playerdata/v1/dep/puid/data/save1.sav HTTP/1.1\r\n" API "Content-Length: 4\r\n\r\nDATA") == 0);
    CHECK(c.nev == 1 && has_event(&c, 0, "PUT", "/playerdata/v1/dep/puid/data/save1.sav"));
    /* a blocked connection stays blocked */
    CHECK(w(&c, "GET /sdk/v1/default HTTP/1.1\r\n" API "\r\n") == 0);
    CHECK(c.mode == EF_MODE_BLOCKED);
}

static void sdk_routes_blocked(void) {
    static const char *const blocked[] = {
        "POST /achievements/v1/dep/achievements/puid/unlock",
        "GET /achievements/v2/dep/definitions/achievements",
        "POST /stats/v1/dep/stats/puid",
        "GET /leaderboards/v1/dep/leaderboards/top",
        "GET /playerdata/v1/dep/puid/access/save.sav",
        "POST /telemetry/data/datarouter/api/v1/public/data",
        "POST /epic/ecom/v1/identities/abc/entitlements/redeem",
        "GET /epic/ecom/v1/identities/abc/namespaces/ns/offers",
        "POST /epic/ecom/v1/purchaseToken",
        "POST /auth/v1/users",
        "POST /auth/v1/accounts/deviceid",
        "POST /auth/v1/turn/credentials",
        "POST /epic/oauth/v1/deviceAuthorization",
        "GET /epic/id/v1/accounts",
        "GET /user/v1/accounts/search",
        "PUT /presence/v1/ns/acc/presence/conn",
        "GET /friends/v1/acc/friends",
        "POST /sessions/v1/dep/sessions",
        /* methods matter */
        "GET /epic/oauth/v1/token",
        "DELETE /epic/ecom/v1/identities/abc/entitlements",
        "HEAD /sdk/v1/default",
        /* path tricks never match */
        "POST /epic/oauth/v1/token/../../../playerdata/v1/x",
        "POST /epic/oauth/v1//token",
        "POST /epic/oauth/%76%31/token",
        "POST /epic/oauth/v1/./token",
        "POST /epic/oauth/v1/token/",
        "POST /epic/oauth/v1/token/extra",
        "POST /epic/oauth/v100/token",
        "POST /Epic/oauth/v1/token",
        "GET /sdk/v1/product/",
        "GET /sdk/v1/product/a/b",
    };
    for (size_t i = 0; i < sizeof blocked / sizeof *blocked; i++) {
        char req[512];
        snprintf(req, sizeof req, "%s HTTP/1.1\r\n" API "\r\n", blocked[i]);
        ef_conn c;
        ef_init(&c, "api.epicgames.dev");
        int v = w(&c, req);
        if (v != 0) fprintf(stderr, "  should be blocked: %s\n", blocked[i]);
        CHECK(v == 0);
    }
    static const char *const allowed[] = {
        "GET /sdk/v1/default?platformId=WIN",
        "GET /sdk/v1/product/prod123?platformId=WIN&deploymentId=dep",
        "POST /epic/oauth/v1/token",
        "POST /epic/oauth/v2/token",
        "POST /epic/oauth/v1/tokenInfo",
        "POST /epic/oauth/v1/revoke",
        "GET /epic/oauth/v2/.well-known/jwks.json",
        "POST /auth/v1/oauth/token",
        "GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item",
        "POST /epic/ecom/v3/platforms/EPIC/identities/abc/ownershipToken",
        "GET /epic/ecom/v4/identities/abc/entitlements?sandboxId=x",
        "POST /epic/ecom/v4/platforms/EPIC/identities/abc/entitlementToken",
        "GET /ecom/v1/identities/abc/entitlements",
    };
    for (size_t i = 0; i < sizeof allowed / sizeof *allowed; i++) {
        char req[512];
        snprintf(req, sizeof req, "%s HTTP/1.1\r\n" API "\r\n", allowed[i]);
        ef_conn c;
        ef_init(&c, "api.epicgames.dev");
        int v = w(&c, req);
        if (v != 1) fprintf(stderr, "  should be allowed: %s\n", allowed[i]);
        CHECK(v == 1);
    }
}

/* Every allowlist entry matches a path made from its own pattern. */
static void every_entry_is_reachable(void) {
    for (size_t i = 0; i < EF_ALLOWLIST_LEN; i++) {
        char path[256] = "";
        const char *p = EF_ALLOWLIST[i].pattern;
        size_t o = 0;
        while (*p && o < sizeof path - 8) {
            if (!strncmp(p, "{v}", 3)) { memcpy(path + o, "v2", 2); o += 2; p += 3; }
            else if (!strncmp(p, "{}", 2)) { memcpy(path + o, "id7", 3); o += 3; p += 2; }
            else path[o++] = *p++;
        }
        path[o] = 0;
        const char *rule = ef_rule(EF_ALLOWLIST[i].method, path, o);
        if (!rule) fprintf(stderr, "  entry %zu (%s %s) doesn't match %s\n", i, EF_ALLOWLIST[i].method, EF_ALLOWLIST[i].pattern, path);
        CHECK(rule != NULL);
        CHECK(EF_ALLOWLIST[i].why && *EF_ALLOWLIST[i].why);
    }
}

static void query_values_never_logged(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /telemetry/data/datarouter/api/v1/public/data?SessionID=s3cr3t&AppID=EOSSDK&UserID=me HTTP/1.1\r\n" API "\r\n") == 0);
    CHECK(c.nev == 1);
    CHECK(!strstr(c.ev[0].target, "s3cr3t") && !strstr(c.ev[0].target, "EOSSDK"));
    CHECK(!strcmp(c.ev[0].target, "/telemetry/data/datarouter/api/v1/public/data?SessionID=\xe2\x80\xa6&AppID=\xe2\x80\xa6&UserID=\xe2\x80\xa6"));
}

/* Headers and bodies split across writes, after a request line that came whole. */
static void split_writes(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /auth/v1/oauth/token HTTP/1.1\r\nHost: api.epic") == 1);
    CHECK(c.nev == 1 && c.ev[0].allowed);
    CHECK(w(&c, "games.dev\r\nContent-Le") == 1);
    CHECK(c.nev == 0);
    CHECK(w(&c, "ngth: 10\r\n\r\n0123") == 1);
    /* the body's end and the next request in one write: an allowed one */
    CHECK(w(&c, "456789GET /sdk/v1/default HTTP/1.1\r\n" API "\r\n") == 1);
    CHECK(c.nev == 1 && has_event(&c, 1, "GET", "/sdk/v1/default"));
    /* a body that looks like a request isn't one */
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Content-Length: 41\r\n\r\n") == 1);
    CHECK(w(&c, "GET /playerdata/v1/x HTTP/1.1\r\nHost: x\r\n\r\n") == 1);
    CHECK(c.nev == 0);
    /* then a blocked one after the body */
    CHECK(w(&c, "GET /stats/v1/dep/stats/puid HTTP/1.1\r\n" API "\r\n") == 0);
    CHECK(has_event(&c, 0, "GET", "/stats/v1/dep/stats/puid"));

    /* byte-at-a-time headers and body */
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v2/token HTTP/1.1\r\n") == 1);
    const char *rest = API "Content-Length: 3\r\n\r\nabc";
    for (const char *p = rest; *p; p++) {
        char one[2] = { *p, 0 };
        CHECK(w(&c, one) == 1);
    }
    CHECK(w(&c, "POST /achievements/v1/d/achievements/p/unlock HTTP/1.1\r\n" API "\r\n") == 0);

    /* one write in two data buffers: the request line may span them */
    ef_init(&c, "api.epicgames.dev");
    CHECK(w2(&c, "GET /sdk/v1/def", "ault HTTP/1.1\r\n" API "\r\n") == 1);
    CHECK(has_event(&c, 1, "GET", "/sdk/v1/default"));
}

/* A request line split across writes can't be decided before its first bytes go: blocked. */
static void split_request_line_is_blocked(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/to") == 0);
    CHECK(c.nev == 1 && !c.ev[0].allowed && !strcmp(c.ev[0].method, "POST"));
    CHECK(w(&c, "ken HTTP/1.1\r\n" API "\r\n") == 0);
    /* after a body, the next request's line split across writes */
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /auth/v1/oauth/token HTTP/1.1\r\n" API "Content-Length: 2\r\n\r\nab") == 1);
    CHECK(w(&c, "GET /sdk/v1/de") == 0);
}

static void pipelining(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "GET /sdk/v1/default HTTP/1.1\r\n" API "\r\nPOST /epic/oauth/v1/token HTTP/1.1\r\n" API "Content-Length: 2\r\n\r\nhi"
                "GET /epic/ecom/v1/platforms/EPIC/identities/a/ownership HTTP/1.1\r\n" API "\r\n") == 1);
    CHECK(c.nev == 3 && c.ev[0].allowed && c.ev[1].allowed && c.ev[2].allowed);
    /* an allowed request and a blocked one in one write: the whole write is blocked */
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "GET /sdk/v1/default HTTP/1.1\r\n" API "\r\nPUT /playerdata/v1/d/p/data/s HTTP/1.1\r\n" API "Content-Length: 1\r\n\r\nx") == 0);
    CHECK(c.nev == 2 && c.ev[0].allowed && !c.ev[1].allowed);
}

static void chunked(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Transfer-Encoding: chunked\r\n\r\n5\r\nab") == 1);
    CHECK(w(&c, "cde\r\n1f;ext=1\r\nGET /playerdata/v1/x HTTP/1.1\r\n") == 1); /* 31 bytes of chunk data */
    CHECK(w(&c, "\r\n0\r\nTrailer: x\r\n\r\nGET /sdk/v1/default HTTP/1.1\r\n" API "\r\n") == 1);
    CHECK(c.nev == 1 && c.ev[0].allowed);
    CHECK(w(&c, "GET /playerdata/v1/x HTTP/1.1\r\n" API "\r\n") == 0);
    /* malformed framing: blocked (it could hide a request) */
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Transfer-Encoding: chunked\r\n\r\n") == 1);
    CHECK(w(&c, "zz\r\n") == 0);
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Transfer-Encoding: gzip\r\n\r\n") == 0);
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Content-Length: 3\r\nContent-Length: 30\r\n\r\nabc") == 0);
}

static void other_hosts_pass_untouched(void) {
    ef_conn c;
    ef_init(&c, "example.com");
    CHECK(c.mode == EF_MODE_PASS);
    CHECK(w(&c, "PUT /playerdata/v1/x HTTP/1.1\r\nHost: example.com\r\n\r\n") == 1);
    CHECK(c.nev == 0);
    CHECK(w(&c, "\x16\x03\x01 binary \x00 junk") == 1);
    CHECK(w(&c, "POST /achievements/v1/d/achievements/p/unlock HTTP/1.1\r\n" API "\r\n") == 1); /* SNI decides */
    ef_init(&c, "public-ubiservices.ubi.com");
    CHECK(w(&c, "POST /v3/profiles/sessions HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\n\r\n") == 1);
    CHECK(c.nev == 0);
    /* the filtered name, any case, a port, a trailing dot */
    CHECK(ef_host_filtered("API.EpicGames.DEV:443", 21));
    CHECK(ef_host_filtered(" api.epicgames.dev. ", 20));
    CHECK(!ef_host_filtered("api.epicgames.dev.evil.com", 26));
    CHECK(!ef_host_filtered("xapi.epicgames.dev", 18));
    CHECK(!ef_host_filtered("epicgames.dev", 13));
}

static void unknown_server(void) {
    ef_conn c;
    /* Host says another server: passed, not logged, and the connection is left alone */
    ef_init(&c, NULL);
    CHECK(w(&c, "PUT /playerdata/v1/x HTTP/1.1\r\nHost: cdn.example.com\r\nContent-Length: 2\r\n\r\nab") == 1);
    CHECK(c.nev == 0 && c.mode == EF_MODE_PASS);
    /* Host says Epic: the allowlist */
    ef_init(&c, NULL);
    CHECK(w(&c, "PUT /playerdata/v1/x HTTP/1.1\r\nHost: api.epicgames.dev\r\n\r\n") == 0);
    CHECK(has_event(&c, 0, "PUT", "/playerdata/v1/x"));
    ef_init(&c, NULL);
    CHECK(w(&c, "POST /epic/oauth/v1/token HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 1\r\n\r\nx") == 1);
    CHECK(c.mode == EF_MODE_FILTER);
    CHECK(w(&c, "POST /stats/v1/d/stats/p HTTP/1.1\r\nHost: anything\r\n\r\n") == 0);
    /* no Host header: an unlisted path is blocked */
    ef_init(&c, NULL);
    CHECK(w(&c, "PUT /playerdata/v1/x HTTP/1.1\r\nContent-Length: 0\r\n\r\n") == 0);
    /* the Host header not in the write that started the request: blocked */
    ef_init(&c, NULL);
    CHECK(w(&c, "PUT /playerdata/v1/x HTTP/1.1\r\nUser-Agent: x\r\n") == 0);
    /* absolute-form names the host */
    ef_init(&c, NULL);
    CHECK(w(&c, "PUT https://api.epicgames.dev/playerdata/v1/x HTTP/1.1\r\n\r\n") == 0);
    ef_init(&c, NULL);
    CHECK(w(&c, "GET https://api.epicgames.dev/sdk/v1/default HTTP/1.1\r\n\r\n") == 1);
    ef_init(&c, NULL);
    CHECK(w(&c, "GET http://other.example/playerdata HTTP/1.1\r\n\r\n") == 1);
    /* not HTTP/1.x: not ours to judge */
    ef_init(&c, NULL);
    CHECK(w(&c, "\x16\x03\x01 binary") == 1 && c.mode == EF_MODE_PASS);
    ef_init(&c, NULL);
    CHECK(wn(&c, "\x00\x00\x12\x04", 4) == 1 && c.mode == EF_MODE_PASS);
    ef_init(&c, NULL);
    CHECK(w(&c, "hello, world") == 1 && c.mode == EF_MODE_PASS);
}

static void not_http_on_epic_is_blocked(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n") == 0);
    ef_init(&c, "api.epicgames.dev");
    CHECK(wn(&c, "\x00\x00\x12\x04\x00\x00\x00\x00\x00\n", 10) == 0);
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, "get /sdk/v1/default HTTP/1.1\r\n\r\n") == 0);
    ef_init(&c, "api.epicgames.dev");
    char longline[4096];
    memset(longline, 'a', sizeof longline);
    memcpy(longline, "GET /", 5);
    longline[sizeof longline - 1] = 0;
    CHECK(w(&c, longline) == 0);
    /* an oversized framing header */
    ef_init(&c, "api.epicgames.dev");
    char req[1024];
    int n = snprintf(req, sizeof req, "POST /epic/oauth/v1/token HTTP/1.1\r\n" API "Content-Length: ");
    memset(req + n, '0', 300);
    strcpy(req + n + 300, "5\r\n\r\nabcde");
    CHECK(w(&c, req) == 0);
    /* a long ordinary header (a bearer token) is fine */
    ef_init(&c, "api.epicgames.dev");
    n = snprintf(req, sizeof req, "GET /epic/ecom/v1/identities/a/entitlements HTTP/1.1\r\n" API "Authorization: Bearer ");
    memset(req + n, 'k', 600);
    strcpy(req + n + 600, "\r\n\r\n");
    CHECK(w(&c, req) == 1);
    CHECK(w(&c, "GET /sdk/v1/default HTTP/1.1\r\n" API "\r\n") == 1);
}

static void many_requests_in_one_write(void) {
    ef_conn c;
    ef_init(&c, "api.epicgames.dev");
    char buf[8192] = "";
    for (int i = 0; i < 12; i++) strcat(buf, "GET /sdk/v1/default HTTP/1.1\r\n" API "\r\n");
    CHECK(w(&c, buf) == 1);
    CHECK(c.nev == EF_EVENTS && c.ev_lost == 12 - EF_EVENTS);
    strcat(buf, "GET /stats/v1/d/stats/p HTTP/1.1\r\n" API "\r\n");
    ef_init(&c, "api.epicgames.dev");
    CHECK(w(&c, buf) == 0);
}

int main(void) {
    allowed_and_blocked();
    sdk_routes_blocked();
    every_entry_is_reachable();
    query_values_never_logged();
    split_writes();
    split_request_line_is_blocked();
    pipelining();
    chunked();
    other_hosts_pass_untouched();
    unknown_server();
    not_http_on_epic_is_blocked();
    many_requests_in_one_write();
    printf("%d checks, %d failed\n", checks, failures);
    return failures != 0;
}
