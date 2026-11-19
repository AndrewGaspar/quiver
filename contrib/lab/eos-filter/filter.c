/* The EOS request filter's parser and allowlist (filter.h). */
#include "filter.h"
#include "allowlist.h"

#include <string.h>

enum {
    S_START,      /* between requests (blank lines are skipped) */
    S_LINE,       /* in a request line */
    S_HEADERS,    /* in a request's header lines */
    S_BODY,       /* in a Content-Length body */
    S_CHUNK_SIZE, /* chunked body: a chunk's size */
    S_CHUNK_EXT,  /* its extensions, up to the line's end */
    S_CHUNK_DATA, /* a chunk's bytes */
    S_CHUNK_CR,   /* the CRLF after them */
    S_CHUNK_LF,
    S_TRAILERS,   /* trailer lines after the last chunk */
};

static int lower(int ch) { return ch >= 'A' && ch <= 'Z' ? ch + 32 : ch; }

static int eq_nocase(const char *a, size_t an, const char *b) {
    size_t bn = strlen(b);
    if (an != bn) return 0;
    for (size_t i = 0; i < an; i++)
        if (lower((unsigned char)a[i]) != lower((unsigned char)b[i])) return 0;
    return 1;
}

int ef_host_filtered(const char *host, size_t n) {
    if (!host) return 0;
    while (n && (*host == ' ' || *host == '\t')) host++, n--;
    while (n && (host[n - 1] == ' ' || host[n - 1] == '\t')) n--;
    if (n && host[0] == '[') return 0; /* an IPv6 literal is never a filtered name */
    for (size_t i = 0; i < n; i++)
        if (host[i] == ':') { n = i; break; }
    if (n && host[n - 1] == '.') n--;
    for (size_t i = 0; i < EF_FILTERED_HOSTS_LEN; i++)
        if (eq_nocase(host, n, EF_FILTERED_HOSTS[i])) return 1;
    return 0;
}

/* A pattern segment against a path segment (allowlist.h's syntax). */
static int seg_match(const char *pat, size_t pn, const char *s, size_t sn) {
    if (pn == 2 && !memcmp(pat, "{}", 2)) return sn > 0;
    if (pn == 3 && !memcmp(pat, "{v}", 3)) {
        if (sn < 2 || sn > 3 || s[0] != 'v') return 0;
        for (size_t i = 1; i < sn; i++)
            if (s[i] < '0' || s[i] > '9') return 0;
        return 1;
    }
    return pn == sn && !memcmp(pat, s, sn);
}

static int path_match(const char *pat, const char *path, size_t n) {
    size_t pl = strlen(pat);
    if (!pl || pat[0] != '/' || !n || path[0] != '/') return 0;
    size_t pi = 1, si = 1;
    for (;;) {
        size_t pe = pi, se = si;
        while (pe < pl && pat[pe] != '/') pe++;
        while (se < n && path[se] != '/') se++;
        if (!seg_match(pat + pi, pe - pi, path + si, se - si)) return 0;
        int pat_done = pe >= pl, path_done = se >= n;
        if (pat_done || path_done) return pat_done && path_done;
        pi = pe + 1, si = se + 1;
    }
}

/* Paths that never match: escapes, backslashes, controls, empty, "." and ".." segments. */
static int path_plain(const char *p, size_t n) {
    if (!n || p[0] != '/') return 0;
    size_t seg = 1;
    for (size_t i = 1; i <= n; i++) {
        if (i == n || p[i] == '/') {
            size_t len = i - seg;
            if (len == 0) return 0;
            if (len == 1 && p[seg] == '.') return 0;
            if (len == 2 && p[seg] == '.' && p[seg + 1] == '.') return 0;
            seg = i + 1;
            continue;
        }
        unsigned char ch = (unsigned char)p[i];
        if (ch <= ' ' || ch >= 0x7f || ch == '%' || ch == '\\') return 0;
    }
    return 1;
}

const char *ef_rule(const char *method, const char *path, size_t n) {
    if (!method || !path || !path_plain(path, n)) return NULL;
    for (size_t i = 0; i < EF_ALLOWLIST_LEN; i++)
        if (!strcmp(method, EF_ALLOWLIST[i].method) && path_match(EF_ALLOWLIST[i].pattern, path, n))
            return EF_ALLOWLIST[i].why;
    return NULL;
}

void ef_init(ef_conn *c, const char *sni) {
    memset(c, 0, sizeof *c);
    c->mode = !sni ? EF_MODE_UNKNOWN : ef_host_filtered(sni, strlen(sni)) ? EF_MODE_FILTER : EF_MODE_PASS;
    c->state = S_START;
    c->verdict = 1;
    c->req_ev = -1;
}

static ef_event *add_event(ef_conn *c, int allowed, const char *why) {
    if (c->nev >= EF_EVENTS) {
        c->ev_lost++;
        return NULL;
    }
    ef_event *e = &c->ev[c->nev++];
    memset(e, 0, sizeof *e);
    e->allowed = allowed;
    e->why = why;
    strcpy(e->method, "-");
    return e;
}

/* Block the write and the connection, saying why on the current request's event. */
static void block(ef_conn *c, const char *why) {
    if (c->mode == EF_MODE_BLOCKED) return;
    ef_event *e = c->req_ev >= 0 ? &c->ev[c->req_ev] : NULL;
    if (e) {
        if (e->allowed || !e->why || c->host_pending) e->why = why;
        e->allowed = 0;
    } else if ((e = add_event(c, 0, why))) {
        /* The bytes belong to a request decided in an earlier write. */
        strcpy(e->target, c->state == S_LINE || c->state == S_START ? "(a request)" : "(an earlier request's body)");
    }
    c->mode = EF_MODE_BLOCKED;
    c->verdict = 0;
    c->host_pending = 0;
}

/* The event's target: the path, then the query's keys (`?a=…&b=…`), cut to fit. */
static void set_target(ef_event *e, const char *t, size_t n) {
    size_t o = 0, cap = sizeof e->target - 1;
    size_t q = 0;
    while (q < n && t[q] != '?') q++;
    for (size_t i = 0; i < q && o < cap; i++) e->target[o++] = t[i];
    if (q < n) {
        int key = 1;
        for (size_t i = q; i < n && o + 4 < cap; i++) {
            char ch = t[i];
            if (ch == '&' || ch == '?') {
                e->target[o++] = ch;
                key = 1;
            } else if (ch == '=') {
                if (key) memcpy(e->target + o, "=\xe2\x80\xa6", 4), o += 4; /* "=…" */
                key = 0;
            } else if (key) {
                e->target[o++] = ch;
            }
        }
    }
    e->target[o] = 0;
}

/* Could these first bytes of a line still be "METHOD " (1-15 capitals and a space)? */
static int method_prefix(const char *l, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (l[i] == ' ') return i > 0;
        if (l[i] < 'A' || l[i] > 'Z' || i >= 15) return 0;
    }
    return 1;
}

static void request_line(ef_conn *c) {
    char *l = c->line;
    size_t n = c->line_len;
    if (n && l[n - 1] == '\r') n--;
    /* METHOD SP target SP HTTP/1.x */
    size_t m = 0;
    while (m < n && m < 15 && l[m] >= 'A' && l[m] <= 'Z') m++;
    size_t t0 = m + 1, t1 = t0;
    while (t1 < n && l[t1] != ' ') t1++;
    int ok = m > 0 && m < n && l[m] == ' ' && t1 > t0 && t1 + 9 == n && !memcmp(l + t1, " HTTP/1.", 8) && l[n - 1] >= '0' && l[n - 1] <= '9';
    if (!ok) {
        if (c->mode == EF_MODE_UNKNOWN) {
            /* Not HTTP/1.x on a connection whose server we never saw: not ours to judge. */
            c->mode = EF_MODE_PASS;
            return;
        }
        ef_event *e = add_event(c, 0, "not an HTTP/1.x request line");
        if (e) {
            size_t k = 0;
            while (k < n && k < 15 && l[k] > ' ' && l[k] < 0x7f) e->method[k] = l[k], k++;
            e->method[k] = 0;
            if (!k) strcpy(e->method, "?");
            c->req_ev = c->nev - 1;
        }
        block(c, "not an HTTP/1.x request line");
        return;
    }
    const char *t = l + t0;
    size_t tn = t1 - t0;
    /* absolute-form ("https://host/path"): the request names its host. */
    if (tn > 8 && (!memcmp(t, "https://", 8) || !memcmp(t, "http://", 7))) {
        size_t h0 = t[4] == 's' ? 8 : 7, h1 = h0;
        while (h1 < tn && t[h1] != '/' && t[h1] != '?') h1++;
        if (ef_host_filtered(t + h0, h1 - h0)) {
            c->mode = EF_MODE_FILTER;
        } else if (c->mode == EF_MODE_UNKNOWN) {
            c->mode = EF_MODE_PASS;
            return;
        }
        t += h1, tn -= h1;
    }
    size_t pn = 0;
    while (pn < tn && t[pn] != '?') pn++;
    char method[16];
    memcpy(method, l, m);
    method[m] = 0;
    const char *rule = ef_rule(method, t, pn);
    ef_event *e = add_event(c, rule != NULL, rule ? rule : "not on the allowlist");
    if (e) {
        memcpy(e->method, method, m + 1);
        set_target(e, t, tn);
        c->req_ev = c->nev - 1;
    } else {
        c->req_ev = -1;
    }
    c->state = S_HEADERS;
    c->line_len = 0;
    c->line_over = 0;
    c->chunked = c->have_len = 0;
    c->content_length = 0;
    if (!rule) {
        if (c->mode == EF_MODE_UNKNOWN) c->host_pending = 1;
        else block(c, "not on the allowlist");
    }
}

static int parse_len(const char *v, size_t n, unsigned long long *out) {
    while (n && (*v == ' ' || *v == '\t')) v++, n--;
    while (n && (v[n - 1] == ' ' || v[n - 1] == '\t')) n--;
    if (!n || n > 18) return 0;
    unsigned long long x = 0;
    for (size_t i = 0; i < n; i++) {
        if (v[i] < '0' || v[i] > '9') return 0;
        x = x * 10 + (unsigned)(v[i] - '0');
    }
    *out = x;
    return 1;
}

/* Is the last comma-separated coding "chunked"? */
static int last_is_chunked(const char *v, size_t n) {
    while (n && (v[n - 1] == ' ' || v[n - 1] == '\t')) n--;
    size_t s = n;
    while (s && v[s - 1] != ',') s--;
    while (s < n && (v[s] == ' ' || v[s] == '\t')) s++;
    return eq_nocase(v + s, n - s, "chunked");
}

static void header_line(ef_conn *c) {
    const char *l = c->line;
    size_t n = c->line_len;
    if (n && l[n - 1] == '\r' && !c->line_over) n--;
    size_t colon = 0;
    while (colon < n && l[colon] != ':') colon++;
    if (colon == n) return; /* not a field (or a long one cut before its colon): framing unaffected */
    size_t nn = colon;
    while (nn && (l[nn - 1] == ' ' || l[nn - 1] == '\t')) nn--;
    const char *v = l + colon + 1;
    size_t vn = n - colon - 1;
    int is_host = eq_nocase(l, nn, "host"), is_len = eq_nocase(l, nn, "content-length"), is_te = eq_nocase(l, nn, "transfer-encoding");
    if ((is_host || is_len || is_te) && c->line_over) {
        block(c, "an oversized Host, Content-Length or Transfer-Encoding header");
        return;
    }
    if (is_host) {
        if (c->mode != EF_MODE_UNKNOWN) return; /* the TLS server name decides */
        if (ef_host_filtered(v, vn)) {
            c->mode = EF_MODE_FILTER;
            if (c->host_pending) {
                c->host_pending = 0;
                block(c, "not on the allowlist");
            }
            return;
        }
        /* Another host: not ours. Its request isn't logged, and the connection is left alone. */
        if (c->req_ev >= 0 && c->req_ev == c->nev - 1) c->nev--;
        c->req_ev = -1;
        c->host_pending = 0;
        c->mode = EF_MODE_PASS;
    } else if (is_len) {
        unsigned long long x;
        if (!parse_len(v, vn, &x) || (c->have_len && x != c->content_length)) {
            block(c, "a malformed or conflicting Content-Length");
            return;
        }
        c->have_len = 1;
        c->content_length = x;
    } else if (is_te) {
        if (!last_is_chunked(v, vn)) {
            block(c, "a Transfer-Encoding other than chunked");
            return;
        }
        c->chunked = 1;
    }
}

static void end_of_head(ef_conn *c) {
    if (c->host_pending) {
        block(c, "not on the allowlist (and no Host header said which server)");
        return;
    }
    c->line_len = 0;
    if (c->chunked) {
        c->state = S_CHUNK_SIZE;
        c->left = 0;
        c->line_len = 0; /* digits seen */
    } else if (c->have_len && c->content_length) {
        c->state = S_BODY;
        c->left = c->content_length;
    } else {
        c->state = S_START;
    }
    c->req_ev = -1;
}

static int hexval(int ch) {
    if (ch >= '0' && ch <= '9') return ch - '0';
    ch = lower(ch);
    if (ch >= 'a' && ch <= 'f') return ch - 'a' + 10;
    return -1;
}

void ef_begin(ef_conn *c) {
    c->nev = c->ev_lost = 0;
    c->req_ev = -1;
    c->verdict = c->mode != EF_MODE_BLOCKED;
    if (c->mode == EF_MODE_BLOCKED) {
        ef_event *e = add_event(c, 0, "this connection already had a blocked request");
        if (e) strcpy(e->target, "(more of the connection)");
    }
}

void ef_data(ef_conn *c, const unsigned char *p, size_t n) {
    size_t i = 0;
    while (i < n) {
        if (c->mode == EF_MODE_PASS || c->mode == EF_MODE_BLOCKED) return;
        unsigned char ch = p[i];
        switch (c->state) {
        case S_START:
            if (ch == '\r' || ch == '\n') { i++; break; }
            c->state = S_LINE;
            c->line_len = 0;
            c->host_pending = 0;
            c->req_ev = -1;
            /* fall through */
        case S_LINE:
            i++;
            if (ch == '\n') {
                request_line(c);
                break;
            }
            if (c->line_len >= EF_LINE_MAX - 1) {
                block(c, "a request line too long to judge");
                return;
            }
            c->line[c->line_len++] = (char)ch;
            if (!method_prefix(c->line, c->line_len)) {
                /* Not an HTTP/1.x request: judged now, whether or not its line ends here. */
                request_line(c);
            }
            break;
        case S_HEADERS:
        case S_TRAILERS:
            i++;
            if (ch == '\n') {
                int blank = !c->line_over && (c->line_len == 0 || (c->line_len == 1 && c->line[0] == '\r'));
                if (blank) {
                    if (c->state == S_HEADERS) end_of_head(c);
                    else c->state = S_START;
                } else if (c->state == S_HEADERS) {
                    header_line(c);
                }
                c->line_len = 0;
                c->line_over = 0;
            } else if (c->line_len < EF_HDR_KEEP) {
                c->line[c->line_len++] = (char)ch;
            } else {
                c->line_over = 1;
            }
            break;
        case S_BODY: {
            size_t k = n - i;
            if (k > c->left) k = (size_t)c->left;
            i += k;
            c->left -= k;
            if (!c->left) c->state = S_START;
            break;
        }
        case S_CHUNK_SIZE: {
            i++;
            int d = hexval(ch);
            if (d >= 0) {
                if (c->left > (~0ULL >> 4)) { block(c, "a chunk size too large"); return; }
                c->left = c->left * 16 + (unsigned)d;
                c->line_len++;
            } else if (ch == ';' || ch == ' ' || ch == '\t') {
                if (!c->line_len) { block(c, "malformed chunked framing"); return; }
                c->state = S_CHUNK_EXT;
            } else if (ch == '\r') {
                /* the line ends at the \n */
            } else if (ch == '\n') {
                if (!c->line_len) { block(c, "malformed chunked framing"); return; }
                c->line_len = 0;
                c->state = c->left ? S_CHUNK_DATA : S_TRAILERS;
            } else {
                block(c, "malformed chunked framing");
                return;
            }
            break;
        }
        case S_CHUNK_EXT:
            i++;
            if (ch == '\n') {
                c->line_len = 0;
                c->state = c->left ? S_CHUNK_DATA : S_TRAILERS;
            }
            break;
        case S_CHUNK_DATA: {
            size_t k = n - i;
            if (k > c->left) k = (size_t)c->left;
            i += k;
            c->left -= k;
            if (!c->left) c->state = S_CHUNK_CR;
            break;
        }
        case S_CHUNK_CR:
            i++;
            if (ch == '\r') c->state = S_CHUNK_LF;
            else if (ch == '\n') { c->state = S_CHUNK_SIZE; c->left = 0; c->line_len = 0; }
            else { block(c, "malformed chunked framing"); return; }
            break;
        case S_CHUNK_LF:
            i++;
            if (ch == '\n') { c->state = S_CHUNK_SIZE; c->left = 0; c->line_len = 0; }
            else { block(c, "malformed chunked framing"); return; }
            break;
        }
    }
}

int ef_end(ef_conn *c) {
    if (c->mode == EF_MODE_FILTER || c->mode == EF_MODE_UNKNOWN) {
        if (c->state == S_LINE) {
            /* A request started in this write without its request line: it can't be decided
             * before its first bytes would be sent. */
            ef_event *e = add_event(c, 0, "a request line split across writes");
            if (e) {
                size_t k = 0;
                while (k < c->line_len && k < 15 && c->line[k] >= 'A' && c->line[k] <= 'Z') e->method[k] = c->line[k], k++;
                e->method[k] = 0;
                if (!k) strcpy(e->method, "?");
                strcpy(e->target, "(incomplete)");
                c->req_ev = c->nev - 1;
            }
            block(c, "a request line split across writes");
        } else if (c->host_pending) {
            block(c, "not on the allowlist (and the Host header wasn't in the same write)");
        }
    }
    return c->verdict;
}
