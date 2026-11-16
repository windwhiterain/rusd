"""Rusd agent client (D10): drive `rusd serve` over stdio JSON-RPC.

A thin, dependency-free (stdlib only) client. One client = one stateful session:
it launches `rusd serve` as a subprocess and speaks newline-delimited JSON-RPC 2.0.
The document is the source of truth; `patch` edits it (D11), and the cook engine
expands generators from it.

Usage:
    from rusd_client import RusdClient
    with RusdClient() as c:
        c.open(path="examples/forest.rusd")
        c.patch([{"op": "set", "prim": "/World/Forest", "property": "count", "value": 100}])
        print(c.expand()["geo_items"])
        img = c.render(width=800, height=600)
        print(c.pick(400, 300)["path"])

Requires a working Python 3 and the `rusd` binary (see RUSD_BIN / target/debug).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


class RusdError(RuntimeError):
    """A JSON-RPC error returned by the server."""

    def __init__(self, error: dict):
        self.code = error.get("code")
        self.message = error.get("message", "")
        super().__init__(f"[{self.code}] {self.message}")


def _find_rusd() -> str:
    env = os.environ.get("RUSD_BIN")
    if env:
        return env
    on_path = shutil.which("rusd")
    if on_path:
        return on_path
    # Walk up from this file to the repo root, then look in target/.
    exe = "rusd.exe" if os.name == "nt" else "rusd"
    here = Path(__file__).resolve()
    for parent in here.parents:
        for profile in ("debug", "release"):
            cand = parent / "target" / profile / exe
            if cand.exists():
                return str(cand)
    raise FileNotFoundError(
        "rusd binary not found; set RUSD_BIN or build it (cargo build -p rusd-cli)"
    )


class RusdClient:
    def __init__(self, rusd_bin: str | None = None):
        self.rusd = rusd_bin or _find_rusd()
        self._id = 0
        self.proc = subprocess.Popen(
            [self.rusd, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )

    # --- transport --------------------------------------------------------

    def call(self, method: str, **params):
        if self.proc.poll() is not None:
            err = self.proc.stderr.read() if self.proc.stderr else ""
            raise RusdError({"code": -32001, "message": f"serve exited: {err}"})
        self._id += 1
        req = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params}
        assert self.proc.stdin and self.proc.stdout
        self.proc.stdin.write(json.dumps(req) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            err = self.proc.stderr.read() if self.proc.stderr else ""
            raise RusdError({"code": -32002, "message": f"no response: {err}"})
        resp = json.loads(line)
        if "error" in resp:
            raise RusdError(resp["error"])
        return resp.get("result")

    # --- tools ------------------------------------------------------------

    def open(self, path: str | None = None, content: str | None = None):
        return self.call("open", **({"path": path} if path else {"content": content}))

    def query(self):
        return self.call("query")

    def validate(self):
        return self.call("validate")

    def expand(self):
        return self.call("expand")

    def render(self, **cfg):
        return self.call("render", **cfg)

    def pick(self, x: int, y: int, **cfg):
        return self.call("pick", x=x, y=y, **cfg)

    def patch(self, ops: list):
        return self.call("patch", ops=ops)

    def set(self, prim: str, property: str, value):
        return self.patch([{"op": "set", "prim": prim, "property": property, "value": value}])

    def define(self, prim: str, schema: str, **props):
        return self.patch([{"op": "define", "prim": prim, "schema": schema, "props": props}])

    def remove(self, prim: str, property: str | None = None):
        op = {"op": "remove", "prim": prim}
        if property is not None:
            op["property"] = property
        return self.patch([op])

    def undo(self):
        return self.call("undo")

    def redo(self):
        return self.call("redo")

    def diff(self):
        return self.call("diff")

    def state(self):
        return self.call("state")

    # --- lifecycle --------------------------------------------------------

    def close(self):
        if self.proc.poll() is None:
            try:
                assert self.proc.stdin
                self.proc.stdin.close()
            except Exception:
                pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


if __name__ == "__main__":
    # Smoke test: open the forest example and report a few facts.
    repo = Path(__file__).resolve().parents[1]
    with RusdClient() as c:
        print("open:", c.open(path=str(repo / "examples" / "forest.rusd")))
        print("expand:", c.expand())
        print("pick(400,300):", c.pick(400, 300))