#!/usr/bin/env python3
"""Probe `rusd serve` (127.0.0.1:8765) directly: preview{solo} on a material layer.

    python probe_solo.py "\\\\?\\C:\\resource\\rusd\\examples\\prototypes.usda"
"""
import json
import socket
import sys
import time

s = socket.create_connection(("127.0.0.1", 8765), timeout=20)
s.settimeout(20)
buf = b""
_id = 0


def read_msg(timeout):
    global buf
    s.settimeout(timeout)
    while b"\n" not in buf:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            return None
        if not chunk:
            raise RuntimeError("EOF")
        buf += chunk
    line, buf = buf.split(b"\n", 1)
    return json.loads(line)


def call(method, params=None, timeout=120):
    global _id
    _id += 1
    req = {"jsonrpc": "2.0", "id": _id, "method": method, "params": params or {}}
    s.sendall((json.dumps(req) + "\n").encode())
    t0 = time.time()
    while True:
        m = read_msg(timeout)
        if m is None:
            return {"timeout_after": round(time.time() - t0, 1)}
        if m.get("id") == _id:
            return m
        print("  notif while waiting:", m.get("method"),
              json.dumps(m.get("params", {}), ensure_ascii=False)[:200])


def trim(v):
    t = json.dumps(v, ensure_ascii=False)
    return t[:600]


def summary(snap):
    p = snap.get("params", {})
    return {k: p.get(k) for k in p if k != "data"}


def main():
    hello = call("hello", {"protocol": 1, "client": "probe"})
    print("hello:", trim(hello))
    print("subscribe:", trim(call("subscribe", {"client": "probe"})))
    for id in sys.argv[1:]:
        print("\n=== solo:", id)
        r = call("preview", {"solo": id}, timeout=180)
        print("reply:", trim(r))
        # the snapshot that follows
        end = time.time() + 30
        while time.time() < end:
            m = read_msg(5)
            if m is None:
                print("no snapshot within 30s")
                break
            if m.get("method") == "stage_snapshot":
                print("snapshot:", trim(summary(m)))
                break
            print("  notif:", m.get("method"), trim(m.get("params")))

    print("\ntree:", trim(call("composition_tree", {}, timeout=120)))


if __name__ == "__main__":
    main()
