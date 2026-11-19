/* A small schannel HTTPS client for the EOS filter's Wine test
 * (crates/quiver-core/tests/eos_filter.rs, wine_*). Talks to a local TLS server only.
 *
 *   client.exe <scenario file>
 *
 * Scenario lines (\r and \n in a write are escapes):
 *   connect <port> <server name> [direct]   a new TLS connection to 127.0.0.1:<port>, the name
 *                                           as SNI; "direct" calls the exported EncryptMessage
 *                                           (as an importer would), else the function table
 *                                           from InitSecurityInterfaceW (as libcurl does)
 *   write <bytes>                           one EncryptMessage of those bytes, then sent
 *   read                                    read one response (status line and body printed)
 *   close
 * Each line's result goes to stdout: "write ok", "write failed 80090329", "read HTTP/1.1 200 OK ...".
 */
#define SECURITY_WIN32
#include <winsock2.h>
#include <windows.h>
#include <schannel.h>
#include <sspi.h>
#include <stdio.h>
#include <string.h>

static PSecurityFunctionTableW T;
static SOCKET S = INVALID_SOCKET;
static CredHandle CRED;
static CtxtHandle CTX;
static int HAVE_CTX, DIRECT;
static SecPkgContext_StreamSizes SIZES;
static char RBUF[1 << 16];
static int RLEN;

static int send_all(const char *p, int n) {
    while (n > 0) {
        int k = send(S, p, n, 0);
        if (k <= 0) return 0;
        p += k, n -= k;
    }
    return 1;
}

static void hangup(void) {
    if (HAVE_CTX) T->DeleteSecurityContext(&CTX);
    HAVE_CTX = 0;
    if (S != INVALID_SOCKET) closesocket(S);
    S = INVALID_SOCKET;
    RLEN = 0;
}

static int do_connect(int port, const char *name) {
    hangup();
    S = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    struct sockaddr_in a = { 0 };
    a.sin_family = AF_INET;
    a.sin_port = htons((u_short)port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(S, (struct sockaddr *)&a, sizeof a)) return printf("connect failed %d\n", WSAGetLastError()), 0;
    DWORD tmo = 8000;
    setsockopt(S, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tmo, sizeof tmo);
    WCHAR wname[256];
    MultiByteToWideChar(CP_UTF8, 0, name, -1, wname, 256);
    DWORD flags = ISC_REQ_SEQUENCE_DETECT | ISC_REQ_REPLAY_DETECT | ISC_REQ_CONFIDENTIALITY | ISC_REQ_ALLOCATE_MEMORY | ISC_REQ_STREAM | ISC_REQ_MANUAL_CRED_VALIDATION;
    SecBuffer ob = { 0, SECBUFFER_TOKEN, NULL };
    SecBufferDesc od = { SECBUFFER_VERSION, 1, &ob };
    ULONG attr;
    SECURITY_STATUS st = T->InitializeSecurityContextW(&CRED, NULL, wname, flags, 0, 0, NULL, 0, &CTX, &od, &attr, NULL);
    if (st != SEC_I_CONTINUE_NEEDED) return printf("connect handshake failed %08lx\n", (unsigned long)st), 0;
    HAVE_CTX = 1;
    if (ob.cbBuffer && !send_all(ob.pvBuffer, (int)ob.cbBuffer)) return printf("connect send failed\n"), 0;
    T->FreeContextBuffer(ob.pvBuffer);
    int len = 0;
    while (st == SEC_I_CONTINUE_NEEDED || st == SEC_E_INCOMPLETE_MESSAGE) {
        int k = recv(S, RBUF + len, (int)sizeof RBUF - len, 0);
        if (k <= 0) return printf("connect recv failed\n"), 0;
        len += k;
        for (;;) {
            SecBuffer ib[2] = { { (ULONG)len, SECBUFFER_TOKEN, RBUF }, { 0, SECBUFFER_EMPTY, NULL } };
            SecBufferDesc id = { SECBUFFER_VERSION, 2, ib };
            SecBuffer ob2 = { 0, SECBUFFER_TOKEN, NULL };
            SecBufferDesc od2 = { SECBUFFER_VERSION, 1, &ob2 };
            st = T->InitializeSecurityContextW(&CRED, &CTX, wname, flags, 0, 0, &id, 0, &CTX, &od2, &attr, NULL);
            if (ob2.cbBuffer && ob2.pvBuffer) {
                if (!send_all(ob2.pvBuffer, (int)ob2.cbBuffer)) return printf("connect send failed\n"), 0;
                T->FreeContextBuffer(ob2.pvBuffer);
            }
            if (st == SEC_E_INCOMPLETE_MESSAGE) break;
            if (ib[1].BufferType == SECBUFFER_EXTRA && ib[1].cbBuffer) {
                memmove(RBUF, RBUF + len - ib[1].cbBuffer, ib[1].cbBuffer);
                len = (int)ib[1].cbBuffer;
                if (st == SEC_I_CONTINUE_NEEDED) continue;
            } else {
                len = 0;
            }
            break;
        }
        if (st != SEC_E_OK && st != SEC_I_CONTINUE_NEEDED && st != SEC_E_INCOMPLETE_MESSAGE)
            return printf("connect handshake failed %08lx\n", (unsigned long)st), 0;
    }
    RLEN = len;
    T->QueryContextAttributesW(&CTX, SECPKG_ATTR_STREAM_SIZES, &SIZES);
    printf("connect ok\n");
    return 1;
}

static void do_write(const char *data, int n) {
    if (!HAVE_CTX) { printf("write failed no-connection\n"); return; }
    char *buf = malloc(SIZES.cbHeader + n + SIZES.cbTrailer);
    memcpy(buf + SIZES.cbHeader, data, n);
    SecBuffer b[4] = {
        { SIZES.cbHeader, SECBUFFER_STREAM_HEADER, buf },
        { (ULONG)n, SECBUFFER_DATA, buf + SIZES.cbHeader },
        { SIZES.cbTrailer, SECBUFFER_STREAM_TRAILER, buf + SIZES.cbHeader + n },
        { 0, SECBUFFER_EMPTY, NULL },
    };
    SecBufferDesc d = { SECBUFFER_VERSION, 4, b };
    SECURITY_STATUS st = DIRECT ? EncryptMessage(&CTX, 0, &d, 0) : T->EncryptMessage(&CTX, 0, &d, 0);
    if (st != SEC_E_OK) {
        /* As libcurl does: nothing is sent, the connection is given up. */
        int zeroed = 1;
        for (int i = 0; i < n; i++) zeroed &= buf[SIZES.cbHeader + i] == 0;
        printf("write failed %08lx%s\n", (unsigned long)st, zeroed ? " (zeroed)" : "");
        free(buf);
        hangup();
        return;
    }
    int ok = send_all(buf, (int)(b[0].cbBuffer + b[1].cbBuffer + b[2].cbBuffer));
    printf(ok ? "write ok\n" : "write send-failed\n");
    free(buf);
}

/* Decrypt what's buffered into plain; returns bytes of plaintext appended, -1 on error. */
static int decrypt_some(char *plain, int cap) {
    int got = 0;
    for (;;) {
        if (RLEN) {
            SecBuffer b[4] = { { (ULONG)RLEN, SECBUFFER_DATA, RBUF }, { 0, SECBUFFER_EMPTY, NULL }, { 0, SECBUFFER_EMPTY, NULL }, { 0, SECBUFFER_EMPTY, NULL } };
            SecBufferDesc d = { SECBUFFER_VERSION, 4, b };
            SECURITY_STATUS st = T->DecryptMessage(&CTX, &d, 0, NULL);
            if (st == SEC_E_OK) {
                int extra = 0;
                for (int i = 0; i < 4; i++) {
                    if (b[i].BufferType == SECBUFFER_DATA && got + (int)b[i].cbBuffer <= cap) {
                        memcpy(plain + got, b[i].pvBuffer, b[i].cbBuffer);
                        got += (int)b[i].cbBuffer;
                    }
                    if (b[i].BufferType == SECBUFFER_EXTRA) extra = (int)b[i].cbBuffer;
                }
                if (extra) memmove(RBUF, RBUF + RLEN - extra, extra);
                RLEN = extra;
                if (got) return got;
                continue;
            }
            if (st != SEC_E_INCOMPLETE_MESSAGE) return -1;
        }
        int k = recv(S, RBUF + RLEN, (int)sizeof RBUF - RLEN, 0);
        if (k <= 0) return got ? got : -1;
        RLEN += k;
    }
}

static void do_read(void) {
    if (!HAVE_CTX) { printf("read failed no-connection\n"); return; }
    static char plain[1 << 16];
    int n = 0;
    char *end = NULL;
    while (!(end = strstr(plain, "\r\n\r\n"))) {
        int k = decrypt_some(plain + n, (int)sizeof plain - n - 1);
        if (k <= 0) { printf("read failed\n"); return; }
        n += k;
        plain[n] = 0;
    }
    int clen = 0;
    char *cl = strstr(plain, "Content-Length: ");
    if (cl && cl < end) clen = atoi(cl + 16);
    int head = (int)(end + 4 - plain);
    while (n < head + clen) {
        int k = decrypt_some(plain + n, (int)sizeof plain - n - 1);
        if (k <= 0) break;
        n += k;
        plain[n] = 0;
    }
    char *eol = strstr(plain, "\r\n");
    printf("read %.*s body=%.*s\n", (int)(eol - plain), plain, clen, plain + head);
    /* Anything past this response stays for the next read. */
    memset(plain, 0, sizeof plain);
}

static int unescape(const char *s, char *out) {
    int n = 0;
    for (; *s && *s != '\n'; s++) {
        if (*s == '\r') continue;
        if (s[0] == '\\' && s[1] == 'r') out[n++] = '\r', s++;
        else if (s[0] == '\\' && s[1] == 'n') out[n++] = '\n', s++;
        else out[n++] = *s;
    }
    return n;
}

int main(int argc, char **argv) {
    if (argc < 2) return fprintf(stderr, "usage: client.exe <scenario>\n"), 2;
    setvbuf(stdout, NULL, _IONBF, 0);
    WSADATA w;
    WSAStartup(MAKEWORD(2, 2), &w);
    T = InitSecurityInterfaceW();
    if (!T) return printf("no security interface\n"), 1;
    SCHANNEL_CRED sc = { 0 };
    sc.dwVersion = SCHANNEL_CRED_VERSION;
    sc.dwFlags = SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_DEFAULT_CREDS;
    sc.grbitEnabledProtocols = SP_PROT_TLS1_2_CLIENT;
    SECURITY_STATUS st = T->AcquireCredentialsHandleW(NULL, (SEC_WCHAR *)UNISP_NAME_W, SECPKG_CRED_OUTBOUND, NULL, &sc, NULL, NULL, &CRED, NULL);
    if (st != SEC_E_OK) return printf("no credentials %08lx\n", (unsigned long)st), 1;
    FILE *f = fopen(argv[1], "rb");
    if (!f) return printf("no scenario %s\n", argv[1]), 1;
    static char line[1 << 15], data[1 << 15];
    while (fgets(line, sizeof line, f)) {
        if (!strncmp(line, "connect ", 8)) {
            int port;
            char name[256], mode[32] = "";
            if (sscanf(line + 8, "%d %255s %31s", &port, name, mode) < 2) continue;
            DIRECT = !strcmp(mode, "direct");
            do_connect(port, name);
        } else if (!strncmp(line, "write ", 6)) {
            do_write(data, unescape(line + 6, data));
        } else if (!strncmp(line, "read", 4)) {
            do_read();
        } else if (!strncmp(line, "close", 5)) {
            hangup();
            printf("close ok\n");
        }
    }
    hangup();
    return 0;
}
