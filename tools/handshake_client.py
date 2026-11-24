#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Minimal test client for the sbcampaign connection handshake.

Plays the client side of the handshake against a running server:

    server -> client  0x1010  Ping
    client -> server  0x2010  Pong
    server -> client  0x1020  XUID request
    client -> server  0x2020  XUID (u64, little endian)
    server -> client  0x3013  Accept   (0x3014 Reject, 0x3015 Retry)

Optionally it then asks for the campaign period (0x1030 -> 0x2030), which
reads the settings table and therefore checks the database round trip.

Frame format (both directions): u16 payload length, u16 command id,
payload; all little endian.

Use synthetic XUIDs only. Standard library only, Python 3.8+.

Exit codes: 0 = handshake accepted, 1 = rejected/unexpected, 2 = network error.
"""

import argparse
import socket
import struct
import sys

PING = 0x1010
PONG = 0x2010
XUID_REQ = 0x1020
XUID_RSP = 0x2020
ACCEPT = 0x3013
REJECT = 0x3014
RETRY = 0x3015
CAMPAIGN_PERIOD_REQ = 0x1030
CAMPAIGN_PERIOD_RSP = 0x2030

NAMES = {
    PING: "Ping", PONG: "Pong", XUID_REQ: "XUID request", XUID_RSP: "XUID",
    ACCEPT: "Accept", REJECT: "Reject", RETRY: "Retry",
    CAMPAIGN_PERIOD_REQ: "CampaignPeriod request",
    CAMPAIGN_PERIOD_RSP: "CampaignPeriod",
}

# Clearly synthetic default (not a real Xbox Live user id).
DEFAULT_XUID = 0x0009FFFF00000001


def name(cmd):
    return "0x%04X %s" % (cmd, NAMES.get(cmd, "?"))


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("server closed the connection")
        buf += chunk
    return buf


def recv_frame(sock):
    length, cmd = struct.unpack("<HH", recv_exact(sock, 4))
    payload = recv_exact(sock, length) if length else b""
    print("<- %s (%d bytes)" % (name(cmd), length))
    return cmd, payload


def send_frame(sock, cmd, payload=b""):
    sock.sendall(struct.pack("<HH", len(payload), cmd) + payload)
    print("-> %s (%d bytes)" % (name(cmd), len(payload)))


def expect(sock, wanted):
    cmd, payload = recv_frame(sock)
    if cmd != wanted:
        raise RuntimeError("expected %s, got %s" % (name(wanted), name(cmd)))
    return payload


def run(host, port, xuid, timeout, period):
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.settimeout(timeout)
        expect(sock, PING)
        send_frame(sock, PONG)
        expect(sock, XUID_REQ)
        send_frame(sock, XUID_RSP, struct.pack("<Q", xuid))
        cmd, _ = recv_frame(sock)
        if cmd != ACCEPT:
            print("FAIL: handshake not accepted (%s)" % name(cmd))
            return 1
        print("OK: handshake accepted")

        if period:
            send_frame(sock, CAMPAIGN_PERIOD_REQ)
            payload = expect(sock, CAMPAIGN_PERIOD_RSP)
            rnd, turn, seconds = struct.unpack("<HHi", payload[:8])
            print("OK: campaign period round=%d turn=%d seconds=%d" % (rnd, turn, seconds))
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=23457)
    ap.add_argument("--xuid", type=lambda v: int(v, 0), default=DEFAULT_XUID,
                    help="synthetic XUID, e.g. 0x0009FFFF00000001")
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--period", action="store_true",
                    help="after the handshake, request the campaign period")
    args = ap.parse_args()
    try:
        return run(args.host, args.port, args.xuid, args.timeout, args.period)
    except (OSError, ConnectionError) as err:
        print("NETWORK ERROR: %s" % err)
        return 2
    except RuntimeError as err:
        print("FAIL: %s" % err)
        return 1


if __name__ == "__main__":
    sys.exit(main())
