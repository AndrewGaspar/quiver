/* The EOS request filter's decisions: a streaming HTTP/1.x request parser per TLS
 * connection and the allowlist (allowlist.h). Portable C with no Windows in it, so it is
 * unit-tested natively (test_filter.c); shim.c is the secur32.dll around it.
 *
 * A connection is fed one write at a time (the plaintext of one EncryptMessage call):
 * ef_begin, ef_data for each data buffer, ef_end. ef_end says whether the write may be
 * encrypted and sent. Each request's verdict is made from its request line (and, when the
 * connection's server name isn't known, its Host header) before any byte of the request is
 * sent: a request that starts in a write is decided within that write, or the write is
 * blocked. Once a request is blocked the connection is: every later write fails.
 */
#ifndef QUIVER_EOS_FILTER_H
#define QUIVER_EOS_FILTER_H

#include <stddef.h>

#define EF_LINE_MAX 2048 /* the longest request line accepted (longer: blocked) */
#define EF_HDR_KEEP 128  /* the bytes of a header line looked at (longer lines are skipped past) */
#define EF_EVENTS 8      /* decisions remembered per write (more are counted) */

enum ef_mode {
    EF_MODE_FILTER,  /* the server is a filtered host: every request passes the allowlist */
    EF_MODE_UNKNOWN, /* the server name was never seen: requests' Host headers decide */
    EF_MODE_PASS,    /* another host: nothing is parsed, everything is sent */
    EF_MODE_BLOCKED, /* a request was blocked: every write fails */
};

typedef struct ef_event {
    int allowed;
    char method[16];
    char target[320]; /* the path, and the query's keys only (values can be ids or codes) */
    const char *why;  /* the allowlist entry, or why it was blocked */
} ef_event;

typedef struct ef_conn {
    int mode;
    int state;
    char line[EF_LINE_MAX];
    size_t line_len;
    int line_over;
    /* The current request. */
    int host_pending; /* unknown server, path not allowed: blocked unless Host is another host */
    int chunked, have_len;
    unsigned long long content_length, left;
    int req_ev;       /* its event in this write, or -1 */
    /* This write. */
    int verdict;      /* 1 send, 0 block */
    ef_event ev[EF_EVENTS];
    int nev, ev_lost;
} ef_conn;

/* sni: the TLS server name the connection was made for, or NULL when unknown. */
void ef_init(ef_conn *c, const char *sni);
/* Is this host (any case, an optional :port or trailing dot) one the filter applies to? */
int ef_host_filtered(const char *host, size_t n);
/* The allowlist entry for a request (path without its query), or NULL: blocked. */
const char *ef_rule(const char *method, const char *path, size_t path_len);

void ef_begin(ef_conn *c);
void ef_data(ef_conn *c, const unsigned char *p, size_t n);
/* 1: send the write; 0: block it (nothing of it may be sent). The write's decisions are in
 * c->ev[0..c->nev) (c->ev_lost more weren't kept); an allowed request in a blocked write
 * isn't sent either. */
int ef_end(ef_conn *c);

#endif
