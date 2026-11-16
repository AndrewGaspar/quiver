// secur32.dll tap: logs plaintext of TLS records whose HTTP request line contains a
// keyword (default "playsession"), plus the responses on the same connection.
// Authorization headers and ticket-like JSON fields are masked before writing.
// Log: $TLS_TAP_LOG, else tls-tap.log next to this DLL. Keyword: $TLS_TAP_MATCH.
#define SECURITY_WIN32
#include <windows.h>
#include <sspi.h>
#include <stdio.h>
#include <string.h>
#include <ctype.h>

extern const char *TAP_NAMES[];
extern const int TAP_COUNT;
extern void *TAP_REAL[];

typedef SECURITY_STATUS (WINAPI *msg_fn)(PCtxtHandle, ULONG, PSecBufferDesc, ULONG);
typedef SECURITY_STATUS (WINAPI *dec_fn)(PCtxtHandle, PSecBufferDesc, ULONG, PULONG);
static msg_fn real_enc, real_seal;
static dec_fn real_dec, real_unseal;
static PSecurityFunctionTableA (WINAPI *real_initA)(void);
static PSecurityFunctionTableW (WINAPI *real_initW)(void);
static SecurityFunctionTableA tabA;
static SecurityFunctionTableW tabW;
static FILE *LOG;
static HMODULE REAL_MOD;

// Wine's schannel.dll forwards InitSecurityInterfaceW to secur32, which now resolves to
// this tap. When the real secur32 loads its providers it must get its own table back, or
// it treats ours as a separate provider and recurses through it.
static int from_real(void *ret) {
    MEMORY_BASIC_INFORMATION mi;
    return VirtualQuery(ret, &mi, sizeof mi) && (HMODULE)mi.AllocationBase == REAL_MOD;
}
static CRITICAL_SECTION CS;
static char MATCH[64] = "playsession";

#define MAXCTX 256
static struct { ULONG_PTR lo, hi; int on; } CTX[MAXCTX];

static int *ctx_flag(PCtxtHandle h) {
    int free_i = -1;
    for (int i = 0; i < MAXCTX; i++) {
        if (CTX[i].lo == h->dwLower && CTX[i].hi == h->dwUpper && (CTX[i].lo || CTX[i].hi)) return &CTX[i].on;
        if (free_i < 0 && !CTX[i].lo && !CTX[i].hi) free_i = i;
    }
    if (free_i < 0) free_i = (int)(h->dwLower % MAXCTX);
    CTX[free_i].lo = h->dwLower; CTX[free_i].hi = h->dwUpper; CTX[free_i].on = 0;
    return &CTX[free_i].on;
}

static int is_request_line(const char *p, unsigned long n) {
    static const char *m[] = { "GET ", "POST ", "PUT ", "DELETE ", "PATCH ", "HEAD ", "OPTIONS " };
    for (int i = 0; i < 7; i++) { size_t l = strlen(m[i]); if (n >= l && !memcmp(p, m[i], l)) return 1; }
    return 0;
}

static int contains(const char *p, unsigned long n, const char *needle) {
    size_t l = strlen(needle);
    for (unsigned long i = 0; i + l <= n; i++) if (!memcmp(p + i, needle, l)) return 1;
    return 0;
}

// Write a copy of the buffer with secrets masked.
static void write_masked(const char *p, unsigned long n) {
    char *s = malloc(n + 1); if (!s) return;
    memcpy(s, p, n); s[n] = 0;
    // Header lines.
    for (char *line = s; line && *line; ) {
        char *eol = strstr(line, "\r\n");
        if (!_strnicmp(line, "Authorization:", 14) || !_strnicmp(line, "Cookie:", 7) || !_strnicmp(line, "Set-Cookie:", 11)) {
            char *colon = strchr(line, ':');
            for (char *q = colon + 1; q < (eol ? eol : line + strlen(line)); q++) *q = (q == colon + 1) ? ' ' : '*';
        }
        line = eol ? eol + 2 : NULL;
    }
    // JSON fields.
    static const char *keys[] = { "\"ticket\"", "\"rememberMeTicket\"", "\"sessionKey\"", "\"token\"", "\"password\"" };
    for (int k = 0; k < 5; k++) {
        for (char *q = s; (q = strstr(q, keys[k])); ) {
            q += strlen(keys[k]);
            while (*q == ' ' || *q == ':') q++;
            if (*q == '"') { q++; while (*q && *q != '"') *q++ = '*'; }
        }
    }
    fwrite(s, 1, n, LOG);
    free(s);
}

static void log_buf(const char *dir, PCtxtHandle h, const char *p, unsigned long n) {
    SYSTEMTIME t; GetLocalTime(&t);
    fprintf(LOG, "\n=== %02d:%02d:%02d.%03d %s ctx=%lx:%lx len=%lu\n", t.wHour, t.wMinute, t.wSecond,
            t.wMilliseconds, dir, (unsigned long)h->dwLower, (unsigned long)h->dwUpper, n);
    write_masked(p, n);
    fputc('\n', LOG);
    fflush(LOG);
}

static void on_send(PCtxtHandle h, PSecBufferDesc m) {
    if (!LOG || !h || !m) return;
    EnterCriticalSection(&CS);
    int *on = ctx_flag(h);
    for (ULONG i = 0; i < m->cBuffers; i++) {
        SecBuffer *b = &m->pBuffers[i];
        if ((b->BufferType & ~SECBUFFER_ATTRMASK) != SECBUFFER_DATA || !b->pvBuffer) continue;
        const char *p = b->pvBuffer;
        if (is_request_line(p, b->cbBuffer)) {
            *on = contains(p, b->cbBuffer, MATCH);
            // Always note the request line (no headers) so we can see what else runs.
            const char *eol = memchr(p, '\r', b->cbBuffer);
            fprintf(LOG, "# %.*s\n", (int)(eol ? eol - p : (b->cbBuffer < 200 ? b->cbBuffer : 200)), p);
            fflush(LOG);
        }
        if (*on) log_buf(">>> send", h, p, b->cbBuffer);
    }
    LeaveCriticalSection(&CS);
}

static void on_recv(PCtxtHandle h, PSecBufferDesc m) {
    if (!LOG || !h || !m) return;
    EnterCriticalSection(&CS);
    int *on = ctx_flag(h);
    if (*on)
        for (ULONG i = 0; i < m->cBuffers; i++) {
            SecBuffer *b = &m->pBuffers[i];
            if ((b->BufferType & ~SECBUFFER_ATTRMASK) == SECBUFFER_DATA && b->pvBuffer && b->cbBuffer)
                log_buf("<<< recv", h, b->pvBuffer, b->cbBuffer);
        }
    LeaveCriticalSection(&CS);
}

SECURITY_STATUS WINAPI w_EncryptMessage(PCtxtHandle h, ULONG q, PSecBufferDesc m, ULONG s) {
    on_send(h, m); return real_enc(h, q, m, s);
}
SECURITY_STATUS WINAPI w_SealMessage(PCtxtHandle h, ULONG q, PSecBufferDesc m, ULONG s) {
    on_send(h, m); return (real_seal ? real_seal : real_enc)(h, q, m, s);
}
SECURITY_STATUS WINAPI w_DecryptMessage(PCtxtHandle h, PSecBufferDesc m, ULONG s, PULONG q) {
    SECURITY_STATUS r = real_dec(h, m, s, q);
    if (r == SEC_E_OK) on_recv(h, m);
    return r;
}
SECURITY_STATUS WINAPI w_UnsealMessage(PCtxtHandle h, PSecBufferDesc m, ULONG s, PULONG q) {
    SECURITY_STATUS r = (real_unseal ? real_unseal : real_dec)(h, m, s, q);
    if (r == SEC_E_OK) on_recv(h, m);
    return r;
}
PSecurityFunctionTableA WINAPI w_InitSecurityInterfaceA(void) {
    PSecurityFunctionTableA t = real_initA();
    if (!t || from_real(__builtin_return_address(0))) return t;
    tabA = *t;
    tabA.EncryptMessage = (ENCRYPT_MESSAGE_FN)w_EncryptMessage;
    tabA.DecryptMessage = (DECRYPT_MESSAGE_FN)w_DecryptMessage;
    return &tabA;
}
PSecurityFunctionTableW WINAPI w_InitSecurityInterfaceW(void) {
    PSecurityFunctionTableW t = real_initW();
    if (!t || from_real(__builtin_return_address(0))) return t;
    tabW = *t;
    tabW.EncryptMessage = (ENCRYPT_MESSAGE_FN)w_EncryptMessage;
    tabW.DecryptMessage = (DECRYPT_MESSAGE_FN)w_DecryptMessage;
    return &tabW;
}

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID r) {
    (void)r;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    InitializeCriticalSection(&CS);
    HMODULE real = LoadLibraryW(L"C:\\windows\\system32\\secur32.dll");
    if (!real || real == inst) return FALSE;
    REAL_MOD = real;
    for (int i = 0; i < TAP_COUNT; i++) TAP_REAL[i] = (void *)GetProcAddress(real, TAP_NAMES[i]);
    real_enc = (msg_fn)GetProcAddress(real, "EncryptMessage");
    real_dec = (dec_fn)GetProcAddress(real, "DecryptMessage");
    real_seal = (msg_fn)GetProcAddress(real, "SealMessage");
    real_unseal = (dec_fn)GetProcAddress(real, "UnsealMessage");
    real_initA = (void *)GetProcAddress(real, "InitSecurityInterfaceA");
    real_initW = (void *)GetProcAddress(real, "InitSecurityInterfaceW");
    const char *m = getenv("TLS_TAP_MATCH");
    if (m && *m) { strncpy(MATCH, m, sizeof MATCH - 1); }
    char path[MAX_PATH];
    const char *env = getenv("TLS_TAP_LOG");
    if (env && *env) strncpy(path, env, MAX_PATH - 1), path[MAX_PATH - 1] = 0;
    else {
        GetModuleFileNameA(inst, path, MAX_PATH);
        char *slash = strrchr(path, '\\'); if (slash) strcpy(slash + 1, "tls-tap.log");
    }
    LOG = fopen(path, "a");
    if (LOG) {
        char exe[MAX_PATH]; GetModuleFileNameA(NULL, exe, MAX_PATH);
        fprintf(LOG, "--- tap loaded in pid %lu %s (match \"%s\")\n", GetCurrentProcessId(), exe, MATCH);
        if (getenv("TLS_TAP_DEBUG"))
            for (int i = 0; i < TAP_COUNT; i++) {
                MEMORY_BASIC_INFORMATION mi; HMODULE owner = NULL;
                if (TAP_REAL[i] && VirtualQuery(TAP_REAL[i], &mi, sizeof mi)) owner = (HMODULE)mi.AllocationBase;
                char mod[MAX_PATH] = "?"; if (owner) GetModuleFileNameA(owner, mod, MAX_PATH);
                fprintf(LOG, "  %s -> %p %s%s\n", TAP_NAMES[i], TAP_REAL[i], mod, owner == inst ? "  <== SELF" : "");
            }
        fflush(LOG);
    }
    return TRUE;
}
