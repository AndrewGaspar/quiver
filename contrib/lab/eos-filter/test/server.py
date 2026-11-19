#!/usr/bin/env python3
"""A local TLS server for the EOS filter's Wine test (never anything but 127.0.0.1).

    server.py <cert.pem> <key.pem> <log dir>

Prints its port, then answers each HTTP/1.1 request with 200 and "ok <path>". Writes
<log dir>/requests.log (one "<SNI> <request line>" per complete request) and
<log dir>/raw.log (every plaintext byte received, so a test can prove a blocked request's
bytes never arrived, not even in part).
"""
import os, socket, ssl, sys, threading

cert, key, logdir = sys.argv[1:4]
lock = threading.Lock()


def log(name, data):
    with lock, open(os.path.join(logdir, name), "ab") as f:
        f.write(data)


def serve(conn):
    sni = getattr(conn, "sni", None) or "-"
    buf = b""
    try:
        while True:
            chunk = conn.recv(65536)
            if not chunk:
                return
            log("raw.log", chunk)
            buf += chunk
            while b"\r\n\r\n" in buf:
                head, rest = buf.split(b"\r\n\r\n", 1)
                length = 0
                for line in head.split(b"\r\n")[1:]:
                    k, _, v = line.partition(b":")
                    if k.strip().lower() == b"content-length":
                        length = int(v.strip())
                if len(rest) < length:
                    break
                buf = rest[length:]
                reqline = head.split(b"\r\n", 1)[0]
                log("requests.log", sni.encode() + b" " + reqline + b"\n")
                path = reqline.split(b" ")[1] if reqline.count(b" ") >= 2 else b"?"
                body = b"ok " + path
                conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: keep-alive\r\n\r\n%s" % (len(body), body))
    except (OSError, ssl.SSLError):
        return
    finally:
        conn.close()


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.maximum_version = ssl.TLSVersion.TLSv1_2
ctx.load_cert_chain(cert, key)


def on_sni(sock, name, _ctx):
    sock.sni = name


ctx.sni_callback = on_sni
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(16)
print(srv.getsockname()[1], flush=True)
while True:
    raw, _ = srv.accept()
    try:
        conn = ctx.wrap_socket(raw, server_side=True)
    except (OSError, ssl.SSLError):
        raw.close()
        continue
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
