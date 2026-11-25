#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Test client for the Steelbound gateway (TLS + sign-in ticket + original protocol).

Every campaign transaction of the game is its own connection; this client does
the same. Per connection:

    TLS handshake, server key checked against --pin (SHA-256 of the SPKI)
    client -> server  prelude  "SBGW" 01 00 <u16 BE ticket length> <ticket>
    server -> client  reply    "SBGW" 01 <status>   0 ok, 1 refused, 2 busy, 3 unsupported
    then the original protocol: 0x1010 Ping / 0x2010 Pong, 0x1020 / 0x2020 qwUserID
    (this client reports a fake one on purpose: the gateway must ignore it),
    0x3013 Accept, one request, one answer.

Cases (--case, default all):

    valid     ticket signed with --key-file: pilot load, register (if new), load,
              save, choose faction, campaign period - all must succeed
    expired   correctly signed, expired ticket       -> must be refused
    forged    signed with a random (unknown) key     -> must be refused
    tampered  valid ticket with one changed byte     -> must be refused
    missing   prelude without a ticket               -> must be refused

Signing needs the Python package "cryptography" (only for cases that sign).
The ticket key file holds the 32-byte Ed25519 seed as 64 hex digits (format of
the issuer's key file). Use TEST keys and synthetic account ids only.

Exit codes: 0 = every case behaved as expected, 1 = a case failed,
2 = network/TLS error or wrong pin.
"""

import argparse
import hashlib
import hmac
import os
import socket
import ssl
import struct
import sys
import time

MAGIC = b"SBGW"
STATUS = {0: "ok", 1: "refused", 2: "busy", 3: "unsupported"}
FACTIONS = {"hsd": 1, "prf": 2, "rb": 3}
PILOT_LEN = 1816
# Clearly fake qwUserID the client reports; the gateway must not use it.
FAKE_XUID = 0x0009FFFF0000BEEF


class Failure(Exception):
    pass


# ---------------------------------------------------------------- DER / pin
def _tlv(b, i):
    tag, ln = b[i], b[i + 1]
    i += 2
    if ln & 0x80:
        n = ln & 0x7F
        if n == 0 or n > 4:
            raise ValueError("bad DER length")
        ln = int.from_bytes(b[i:i + n], "big")
        i += n
    return tag, i, i + ln


def spki_pin(cert_der):
    """SHA-256 over the DER SubjectPublicKeyInfo, 64 lowercase hex digits."""
    _, start, _ = _tlv(cert_der, 0)              # Certificate
    tag, tbs_start, tbs_end = _tlv(cert_der, start)  # tbsCertificate
    i, index = tbs_start, 0
    while i < tbs_end:
        tag, cstart, cend = _tlv(cert_der, i)
        if index == 0 and tag == 0xA0:           # [0] version
            i = cend
            continue
        if index == 5:                           # subjectPublicKeyInfo
            return hashlib.sha256(cert_der[i:cend]).hexdigest()
        index += 1
        i = cend
    raise ValueError("no SubjectPublicKeyInfo")


# ---------------------------------------------------------------- tickets
def _label(s):
    raw = s.encode("utf-8")
    if not 1 <= len(raw) <= 64:
        raise ValueError("label length")
    return bytes([len(raw)]) + raw


def make_ticket(seed, account, issuer, campaign, display_name, issued_at, expires_at):
    """Ticket format v1 (see the server's src/ticket/mod.rs), Ed25519-signed."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
    key = Ed25519PrivateKey.from_private_bytes(seed)
    pub = key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    body = b"SBCT" + bytes([1]) + pub[:8]
    body += struct.pack(">QQIQ", issued_at, expires_at, account, (0x53424C01 << 32) | account)
    body += os.urandom(16)
    for s in (issuer, "steelbound", campaign, display_name):
        body += _label(s)
    return body + key.sign(body)


def read_seed(path):
    text = open(path, encoding="ascii").read().strip()
    seed = bytes.fromhex(text)
    if len(seed) != 32:
        raise ValueError("key file: expected 64 hex digits")
    return seed


# ---------------------------------------------------------------- connection
class Conn:
    def __init__(self, args):
        self.args = args
        raw = socket.create_connection((args.host, args.port), timeout=args.timeout)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        # Only the pin decides (self-signed server certificate), as in the game client.
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.minimum_version = ssl.TLSVersion.TLSv1_2
        self.s = ctx.wrap_socket(raw, server_hostname=args.sni or None)
        got = spki_pin(self.s.getpeercert(binary_form=True))
        if args.pin:
            if not hmac.compare_digest(got, args.pin.lower()):
                self.s.close()
                raise ConnectionError("server key does not match the pin (got %s)" % got)
        elif not args.quiet:
            print("   (no --pin given; server pin is %s)" % got)

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass

    def recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.s.recv(n - len(buf))
            if not chunk:
                raise Failure("server closed the connection")
            buf += chunk
        return buf

    def prelude(self, ticket):
        self.s.sendall(MAGIC + bytes([1, 0]) + struct.pack(">H", len(ticket)) + ticket)
        reply = self.recv_exact(6)
        if reply[:4] != MAGIC:
            raise Failure("not a gateway reply: %r" % reply)
        return reply[5]

    def send(self, cmd, payload=b""):
        self.s.sendall(struct.pack("<HH", len(payload), cmd) + payload)

    def recv(self):
        length, cmd = struct.unpack("<HH", self.recv_exact(4))
        return cmd, (self.recv_exact(length) if length else b"")

    def expect(self, want):
        cmd, payload = self.recv()
        if cmd != want:
            raise Failure("expected 0x%04X, got 0x%04X" % (want, cmd))
        return payload

    def handshake(self):
        self.expect(0x1010)
        self.send(0x2010)
        self.expect(0x1020)
        self.send(0x2020, struct.pack("<Q", FAKE_XUID))
        cmd, _ = self.recv()
        if cmd != 0x3013:
            raise Failure("handshake not accepted (0x%04X)" % cmd)


def transaction(args, ticket, cmd, payload, answer):
    """One game transaction; returns the answer payload."""
    c = Conn(args)
    try:
        status = c.prelude(ticket)
        if status != 0:
            raise Failure("ticket not accepted (%s)" % STATUS.get(status, status))
        c.handshake()
        c.send(cmd, payload)
        return c.expect(answer)
    finally:
        c.close()


def name16(s):
    raw = s.encode("ascii")[:16]
    return raw + b"\0" * (16 - len(raw))


def pilot_blob(name):
    blob = bytearray(PILOT_LEN)
    raw = name.encode("ascii")[:15]
    blob[40:40 + len(raw)] = raw
    return bytes(blob)


# ---------------------------------------------------------------- cases
def case_valid(args, ticket):
    pilot = args.pilot_name
    p = transaction(args, ticket, 0x1021, b"", 0x2021)
    print("   pilot load: status %d (%s)" % (p[0], "has pilot" if p[0] == 0 else "no pilot yet"))
    if p[0] != 0:
        payload = name16(pilot) + name16("ignored") + bytes([FACTIONS[args.faction]])
        payload += struct.pack("<H", PILOT_LEN) + pilot_blob(pilot)
        p = transaction(args, ticket, 0x5022, payload, 0x6022)
        if p[0] != 0:
            raise Failure("pilot register: status %d" % p[0])
        print("   pilot register: ok")
    p = transaction(args, ticket, 0x1021, b"", 0x2021)
    if p[0] != 0:
        raise Failure("pilot load after register: status %d" % p[0])
    print("   pilot load: ok, name %r, faction %d" % (p[1:17].split(b"\0")[0].decode("ascii", "replace"), p[17]))
    p = transaction(args, ticket, 0x5024, struct.pack("<H", PILOT_LEN) + pilot_blob(pilot), 0x6024)
    if p[0] != 0:
        raise Failure("pilot save: status %d" % p[0])
    print("   pilot save: ok")
    p = transaction(args, ticket, 0x5025, bytes([FACTIONS[args.faction]]), 0x6025)
    print("   faction choose: status %d (1 = reload garage, expected)" % p[0])
    p = transaction(args, ticket, 0x1030, b"", 0x2030)
    rnd, turn, seconds = struct.unpack("<HHi", p[:8])
    print("   campaign period: round %d, turn %d, %d s left" % (rnd, turn, seconds))


def expect_refused(args, ticket):
    c = Conn(args)
    try:
        status = c.prelude(ticket)
    finally:
        c.close()
    print("   prelude status: %s" % STATUS.get(status, status))
    if status != 1:
        raise Failure("expected refused, got %s" % STATUS.get(status, status))


def run_case(args, name, seed):
    now = int(time.time())
    common = dict(account=args.account, issuer=args.issuer, campaign=args.campaign, display_name=args.display_name)
    if name == "missing":
        return expect_refused(args, b"")
    if name == "forged":
        return expect_refused(args, make_ticket(os.urandom(32), issued_at=now - 10, expires_at=now + 1800, **common))
    if seed is None:
        raise Failure("--key-file is needed for case %s" % name)
    if name == "expired":
        return expect_refused(args, make_ticket(seed, issued_at=now - 2000, expires_at=now - 300, **common))
    valid = make_ticket(seed, issued_at=now - 10, expires_at=now + args.ttl, **common)
    if name == "tampered":
        t = bytearray(valid)
        t[30] ^= 0x01  # account id
        return expect_refused(args, bytes(t))
    if args.ticket_file:
        valid = open(args.ticket_file, "rb").read()
    return case_valid(args, valid)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=47100)
    ap.add_argument("--pin", help="expected SPKI pin of the server (64 hex digits)")
    ap.add_argument("--sni", default="", help="TLS server name to send (default none)")
    ap.add_argument("--case", default="all",
                    choices=["all", "valid", "expired", "forged", "tampered", "missing"])
    ap.add_argument("--key-file", help="ticket signing key (TEST key, 64 hex digits)")
    ap.add_argument("--ticket-file", help="use this raw ticket for the valid case instead of signing")
    ap.add_argument("--account", type=int, default=4242, help="synthetic account id")
    ap.add_argument("--issuer", default="steelnet-eu")
    ap.add_argument("--campaign", default="steelbound-test")
    ap.add_argument("--display-name", default="Test Pilot")
    ap.add_argument("--pilot-name", default="SynthPilot")
    ap.add_argument("--faction", default="hsd", choices=sorted(FACTIONS))
    ap.add_argument("--ttl", type=int, default=1800)
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--print-public-key", action="store_true",
                    help="print the public key (64 hex digits) of --key-file for [ticket] public_keys and exit")
    args = ap.parse_args()
    if args.print_public_key:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
        if not args.key_file:
            ap.error("--print-public-key needs --key-file")
        key = Ed25519PrivateKey.from_private_bytes(read_seed(args.key_file))
        print(key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw).hex())
        return 0
    if args.pin and len(args.pin) != 64:
        ap.error("--pin must be 64 hex digits")

    seed = read_seed(args.key_file) if args.key_file else None
    cases = ["valid", "expired", "forged", "tampered", "missing"] if args.case == "all" else [args.case]
    if seed is None and args.case == "all":
        cases = ["forged", "missing"]
        print("(no --key-file: running only the cases that need no key)")
    failed = 0
    for name in cases:
        print("== %s" % name)
        try:
            run_case(args, name, seed)
            print("OK: %s" % name)
        except Failure as err:
            failed += 1
            print("FAIL: %s: %s" % (name, err))
        except (OSError, ConnectionError, ssl.SSLError) as err:
            print("NETWORK ERROR: %s: %s" % (name, err))
            return 2
    print("%d of %d case(s) as expected" % (len(cases) - failed, len(cases)))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
