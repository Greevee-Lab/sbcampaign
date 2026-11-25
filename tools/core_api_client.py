#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Minimal Core API client (Connect-JSON binding over mutual TLS), standard
library only. For smoke tests on a server and in CI.

  core_api_client.py --port 47101 --ca ca.crt --cert c.crt --key c.key \
      steelcore.v1.IdentityService/ResolveIdentity '{"externalId": "1"}'

Prints the HTTP status and the JSON body. Exit code 0 if the call matched
--expect (default: "ok"), 1 otherwise. --expect takes "ok", an error code
such as "permission_denied", or "tls" for a refused TLS connection.
"""
import argparse
import http.client
import json
import ssl
import sys


def call(host, port, ca, cert, key, method, body, timeout=10.0):
    ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH, cafile=ca)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.set_alpn_protocols(["http/1.1"])
    if cert:
        ctx.load_cert_chain(cert, key)
    conn = http.client.HTTPSConnection(host, port, context=ctx, timeout=timeout)
    try:
        conn.request("POST", "/" + method, body=json.dumps(body).encode(),
                     headers={"Content-Type": "application/json", "Connect-Protocol-Version": "1"})
        resp = conn.getresponse()
        return resp.status, json.loads(resp.read() or b"null")
    finally:
        conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--ca", required=True)
    ap.add_argument("--cert")
    ap.add_argument("--key")
    ap.add_argument("--expect", default="ok")
    ap.add_argument("method")
    ap.add_argument("body", nargs="?", default="{}")
    a = ap.parse_args()
    try:
        status, body = call(a.host, a.port, a.ca, a.cert, a.key, a.method, json.loads(a.body))
    except (ssl.SSLError, ConnectionError, http.client.RemoteDisconnected, OSError) as e:
        print("tls/transport refused: %s" % type(e).__name__)
        return 0 if a.expect == "tls" else 1
    print(status, json.dumps(body, sort_keys=True))
    got = "ok" if status == 200 else (body or {}).get("code", "?")
    return 0 if got == a.expect else 1


if __name__ == "__main__":
    sys.exit(main())
