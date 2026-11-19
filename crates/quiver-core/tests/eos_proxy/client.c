/* A Windows HTTPS client for the EOS proxy's Wine test (tests/eos_proxy.rs). It trusts what
 * a game on Unreal's SSL code trusts: the Windows ROOT store (CertOpenSystemStoreW), which the
 * lab fills from the prefix's registry. Talks to 127.0.0.1 only.
 *
 *   client.exe <scenario file>
 *
 * Scenario lines (\r and \n in a write are escapes):
 *   roots <text>                     count the ROOT store's certificates whose subject has <text>
 *   connect <port> <server name>     TLS to 127.0.0.1:<port> with that SNI, then the server's
 *                                    chain checked against the ROOT store and the name (as
 *                                    libcurl-over-schannel and Unreal's OpenSSL setup both do)
 *   write <bytes> | read | close
 * Results go to stdout, one line each: "roots 1", "connect ok verified",
 * "connect untrusted 800b0109", "write ok", "read HTTP/1.1 403 Forbidden body=…".
 */
#define SECURITY_WIN32
#include <winsock2.h>
#include <windows.h>
#include <wincrypt.h>
#include <schannel.h>
#include <sspi.h>
#include <stdio.h>
#include <string.h>

static PSecurityFunctionTableW T;
static SOCKET S = INVALID_SOCKET;
static CredHandle CRED;
static CtxtHandle CTX;
static int HAVE_CTX;
static SecPkgContext_StreamSizes SIZES;
static char RBUF[1 << 16];
static int RLEN;
static char PLAIN[1 << 16];
static int PLEN;

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
    RLEN = PLEN = 0;
}

static void do_roots(const char *text) {
    HCERTSTORE st = CertOpenSystemStoreW(0, L"ROOT");
    if (!st) { printf("roots failed %lu\n", GetLastError()); return; }
    int n = 0;
    PCCERT_CONTEXT c = NULL;
    while ((c = CertEnumCertificatesInStore(st, c))) {
        char name[512];
        CertNameToStrA(X509_ASN_ENCODING, &c->pCertInfo->Subject, CERT_X500_NAME_STR, name, sizeof name);
        if (strstr(name, text)) n++;
    }
    CertCloseStore(st, 0);
    printf("roots %d\n", n);
}

/* The server's chain against the ROOT store, for this name. 0 or the policy error. */
static DWORD verify(const char *name) {
    PCCERT_CONTEXT cert = NULL;
    if (T->QueryContextAttributesW(&CTX, SECPKG_ATTR_REMOTE_CERT_CONTEXT, &cert) != SEC_E_OK || !cert) return 1;
    CERT_CHAIN_PARA para;
    memset(&para, 0, sizeof para);
    para.cbSize = sizeof para;
    PCCERT_CHAIN_CONTEXT chain = NULL;
    DWORD err = 2;
    if (CertGetCertificateChain(NULL, cert, NULL, cert->hCertStore, &para, 0, NULL, &chain)) {
        WCHAR wname[256];
        MultiByteToWideChar(CP_UTF8, 0, name, -1, wname, 256);
        SSL_EXTRA_CERT_CHAIN_POLICY_PARA ssl;
        memset(&ssl, 0, sizeof ssl);
        ssl.cbSize = sizeof ssl;
        ssl.dwAuthType = AUTHTYPE_SERVER;
        ssl.pwszServerName = wname;
        CERT_CHAIN_POLICY_PARA pp;
        memset(&pp, 0, sizeof pp);
        pp.cbSize = sizeof pp;
        pp.pvExtraPolicyPara = &ssl;
        CERT_CHAIN_POLICY_STATUS ps;
        memset(&ps, 0, sizeof ps);
        ps.cbSize = sizeof ps;
        if (CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain, &pp, &ps)) err = ps.dwError;
        CertFreeCertificateChain(chain);
    }
    CertFreeCertificateContext(cert);
    return err;
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
    if (st != SEC_I_CONTINUE_NEEDED) return printf("connect handshake-failed %08lx\n", (unsigned long)st), 0;
    HAVE_CTX = 1;
    if (ob.cbBuffer && !send_all(ob.pvBuffer, (int)ob.cbBuffer)) return printf("connect send-failed\n"), 0;
    T->FreeContextBuffer(ob.pvBuffer);
    int len = 0;
    while (st == SEC_I_CONTINUE_NEEDED || st == SEC_E_INCOMPLETE_MESSAGE) {
        int k = recv(S, RBUF + len, (int)sizeof RBUF - len, 0);
        if (k <= 0) { hangup(); return printf("connect handshake-failed closed\n"), 0; }
        len += k;
        for (;;) {
            SecBuffer ib[2] = { { (ULONG)len, SECBUFFER_TOKEN, RBUF }, { 0, SECBUFFER_EMPTY, NULL } };
            SecBufferDesc id = { SECBUFFER_VERSION, 2, ib };
            SecBuffer ob2 = { 0, SECBUFFER_TOKEN, NULL };
            SecBufferDesc od2 = { SECBUFFER_VERSION, 1, &ob2 };
            st = T->InitializeSecurityContextW(&CRED, &CTX, wname, flags, 0, 0, &id, 0, &CTX, &od2, &attr, NULL);
            if (ob2.cbBuffer && ob2.pvBuffer) {
                if (!send_all(ob2.pvBuffer, (int)ob2.cbBuffer)) return printf("connect send-failed\n"), 0;
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
        if (st != SEC_E_OK && st != SEC_I_CONTINUE_NEEDED && st != SEC_E_INCOMPLETE_MESSAGE) {
            hangup();
            return printf("connect handshake-failed %08lx\n", (unsigned long)st), 0;
        }
    }
    RLEN = len;
    T->QueryContextAttributesW(&CTX, SECPKG_ATTR_STREAM_SIZES, &SIZES);
    DWORD v = verify(name);
    if (v) {
        /* As a verifying client does: an untrusted server gets nothing. */
        printf("connect untrusted %08lx\n", (unsigned long)v);
        hangup();
        return 0;
    }
    printf("connect ok verified\n");
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
    SECURITY_STATUS st = T->EncryptMessage(&CTX, 0, &d, 0);
    int ok = st == SEC_E_OK && send_all(buf, (int)(b[0].cbBuffer + b[1].cbBuffer + b[2].cbBuffer));
    printf(ok ? "write ok\n" : "write failed\n");
    free(buf);
}

/* More plaintext into PLAIN; 0 when the connection ended. */
static int more(void) {
    for (;;) {
        if (RLEN) {
            SecBuffer b[4] = { { (ULONG)RLEN, SECBUFFER_DATA, RBUF }, { 0, SECBUFFER_EMPTY, NULL }, { 0, SECBUFFER_EMPTY, NULL }, { 0, SECBUFFER_EMPTY, NULL } };
            SecBufferDesc d = { SECBUFFER_VERSION, 4, b };
            SECURITY_STATUS st = T->DecryptMessage(&CTX, &d, 0, NULL);
            if (st == SEC_E_OK) {
                int extra = 0, got = 0;
                for (int i = 0; i < 4; i++) {
                    if (b[i].BufferType == SECBUFFER_DATA && PLEN + (int)b[i].cbBuffer < (int)sizeof PLAIN) {
                        memcpy(PLAIN + PLEN, b[i].pvBuffer, b[i].cbBuffer);
                        PLEN += (int)b[i].cbBuffer;
                        got += (int)b[i].cbBuffer;
                    }
                    if (b[i].BufferType == SECBUFFER_EXTRA) extra = (int)b[i].cbBuffer;
                }
                if (extra) memmove(RBUF, RBUF + RLEN - extra, extra);
                RLEN = extra;
                PLAIN[PLEN] = 0;
                if (got) return 1;
                continue;
            }
            if (st != SEC_E_INCOMPLETE_MESSAGE) return 0;
        }
        int k = recv(S, RBUF + RLEN, (int)sizeof RBUF - RLEN, 0);
        if (k <= 0) return 0;
        RLEN += k;
    }
}

/* One response; anything after it stays for the next read (pipelining). */
static void do_read(void) {
    if (!HAVE_CTX) { printf("read failed no-connection\n"); return; }
    char *end;
    while (!(end = strstr(PLAIN, "\r\n\r\n")))
        if (!more()) { printf("read failed\n"); return; }
    int head = (int)(end + 4 - PLAIN), clen = 0;
    for (char *p = PLAIN; p < end; p = strstr(p, "\r\n") + 2)
        if (!_strnicmp(p, "content-length:", 15)) clen = atoi(p + 15);
    while (PLEN < head + clen)
        if (!more()) break;
    char *eol = strstr(PLAIN, "\r\n");
    printf("read %.*s body=%.*s\n", (int)(eol - PLAIN), PLAIN, clen, PLAIN + head);
    int used = head + clen < PLEN ? head + clen : PLEN;
    memmove(PLAIN, PLAIN + used, PLEN - used);
    PLEN -= used;
    PLAIN[PLEN] = 0;
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
    SCHANNEL_CRED sc = { 0 };
    sc.dwVersion = SCHANNEL_CRED_VERSION;
    sc.dwFlags = SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_DEFAULT_CREDS;
    sc.grbitEnabledProtocols = SP_PROT_TLS1_2_CLIENT;
    if (T->AcquireCredentialsHandleW(NULL, (SEC_WCHAR *)UNISP_NAME_W, SECPKG_CRED_OUTBOUND, NULL, &sc, NULL, NULL, &CRED, NULL) != SEC_E_OK)
        return printf("no credentials\n"), 1;
    FILE *f = fopen(argv[1], "rb");
    if (!f) return printf("no scenario %s\n", argv[1]), 1;
    static char line[1 << 15], data[1 << 15];
    while (fgets(line, sizeof line, f)) {
        line[strcspn(line, "\r\n")] = 0;
        if (!strncmp(line, "roots ", 6)) {
            do_roots(line + 6);
        } else if (!strncmp(line, "connect ", 8)) {
            int port;
            char name[256];
            if (sscanf(line + 8, "%d %255s", &port, name) == 2) do_connect(port, name);
        } else if (!strncmp(line, "write ", 6)) {
            do_write(data, unescape(line + 6, data));
        } else if (!strcmp(line, "read")) {
            do_read();
        } else if (!strcmp(line, "close")) {
            hangup();
            printf("close ok\n");
        }
    }
    hangup();
    return 0;
}
