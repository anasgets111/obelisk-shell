#!/usr/bin/env python3
"""Format Lua with lua-language-server's own formatter, the one an editor already runs.

`just fmt` writes; `just fmt-check` reports and fails. Both go through here.

## Why a script rather than a formatter binary

The Lua half of this repo is the larger half by file count and had no formatting gate at all, so
`just check` stayed green over files a reviewer would have sent back. The obvious answer was stylua,
which is one packaged binary and one `--check` flag. It was the wrong one: an editor pointed at
`lua-language-server` (Zed's default for Lua, and this repo's own `just types` dependency) formats on
save with EmmyLuaCodeStyle, and the two disagree. They disagree loudly on `align_continuous_assign
_statement`, which lines up runs of assignments and table fields -- the style every token table in
`config/` is written in. A stylua gate would have flattened those columns on every `just fmt` and an
editor would have put them back on every save.

So the gate runs the formatter the editor runs. Nothing is bundled or vendored here: this drives the
same `lua-language-server` binary through the same request an editor sends, so there is no second
implementation to drift.

## What it speaks

LSP over stdio: `initialize`, one `textDocument/didOpen` and `textDocument/formatting` per file, then
the whole-file `TextEdit` that comes back. One server for the whole run, because startup dominates.

The `initialize` handshake is answered before the workspace has finished loading, and a `formatting`
request sent into that window answers with `null` -- which is indistinguishable from "already
formatted". Hence `READY_GRACE`: a small wait after `initialized`, once per run rather than per file.
"""

import glob
import json
import os
import subprocess
import sys
import time

# What the server is given before the first file. Loading a workspace this size takes under a
# second; the cost is paid once and a formatting request arriving early answers `null`, which reads
# as "no changes" and would let an unformatted file through the gate.
READY_GRACE = 3.0

# A reply must arrive within this. The server is local and answers a formatting request in
# milliseconds, so this is a deadlock backstop, not a budget.
REPLY_TIMEOUT = 30.0

# How many times a file is re-formatted before its output is taken as final.
#
# One pass is not a fixed point. On a file with aligned assignments *and* trailing comments --
# `config/icons.lua` is the one here -- the first pass collapses each comment to one space after the
# value and the second re-aligns the comments into their own column, after which it is stable. A
# formatter that disagrees with its own output makes `just fmt` and `just fmt-check` disagree too:
# the sweep wrote pass one, the gate then asked for pass two, and `just check` was red on a tree
# that had just been formatted. So both modes settle the file first and compare against that.
MAX_PASSES = 4


def find_server():
    """`lua-language-server` on PATH, else the copy an editor extension downloaded for itself.

    The same lookup `just types` does, and for the same reason: on the machine this was written on
    that editor copy is the only one, so a PATH-only search would fail there. Newest wins.
    """
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        candidate = os.path.join(directory, "lua-language-server")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    editor_copies = glob.glob(
        os.path.expanduser("~/.local/share/zed/extensions/work/lua/lua-language-server-*/bin/lua-language-server")
    )
    return max(editor_copies) if editor_copies else None


class Server:
    def __init__(self, binary, root):
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.next_id = 1
        self._request("initialize", {"processId": os.getpid(), "rootUri": "file://" + root, "capabilities": {}})
        self._notify("initialized", {})
        time.sleep(READY_GRACE)

    def _send(self, message):
        body = json.dumps(message).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()

    def _read(self):
        """One LSP message, or None once the server's stdout closes."""
        length = None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                return None
            if line in (b"\r\n", b"\n"):
                break
            if line.lower().startswith(b"content-length"):
                length = int(line.split(b":")[1])
        if length is None:
            return None
        return json.loads(self.proc.stdout.read(length))

    def _notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _request(self, method, params):
        """Send one request and return its result, ignoring the notifications interleaved with it.

        The server talks while it works -- progress, diagnostics, log messages -- and every one of
        those arrives on the same pipe as the reply. Match on `id` rather than reading the next
        message and hoping.
        """
        request_id = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.time() + REPLY_TIMEOUT
        while time.time() < deadline:
            message = self._read()
            if message is None:
                raise SystemExit("lua-language-server closed the connection mid-request")
            if message.get("id") == request_id:
                if "error" in message:
                    raise SystemExit(f"lua-language-server refused {method}: {message['error']}")
                return message.get("result")
        raise SystemExit(f"lua-language-server did not answer {method} within {REPLY_TIMEOUT:.0f}s")

    def _one_pass(self, path, source, version):
        """One formatting round trip. `null` edits mean the server had nothing to change."""
        uri = "file://" + path
        self._notify(
            "textDocument/didOpen",
            {"textDocument": {"uri": uri, "languageId": "lua", "version": version, "text": source}},
        )
        # `tabSize`/`insertSpaces` are the LSP-mandated fields every formatting request carries.
        # Everything else about the style -- alignment, line width, quote handling -- is the
        # server's own, read from `.editorconfig` when a project has one.
        edits = self._request(
            "textDocument/formatting",
            {"textDocument": {"uri": uri}, "options": {"tabSize": 4, "insertSpaces": True}},
        )
        self._notify("textDocument/didClose", {"textDocument": {"uri": uri}})
        if not edits:
            return source
        # One whole-file edit is what this server returns. Anything else would need the ranges
        # applied back to front, and silently taking `[0]` would corrupt the file instead.
        if len(edits) != 1:
            raise SystemExit(f"{path}: expected one whole-file edit, got {len(edits)}")
        return edits[0]["newText"]

    def formatted(self, path, source):
        """The file as re-formatting it stops changing it. See `MAX_PASSES`."""
        text = source
        for version in range(1, MAX_PASSES + 1):
            settled = self._one_pass(path, text, version)
            if settled == text:
                return text
            text = settled
        raise SystemExit(
            f"{path}: still changing after {MAX_PASSES} formatting passes. The formatter is "
            f"oscillating rather than converging, and a gate cannot be built on that."
        )

    def close(self):
        self.proc.kill()


def main():
    check_only = "--check" in sys.argv[1:]
    roots = [argument for argument in sys.argv[1:] if argument != "--check"]

    binary = find_server()
    if binary is None:
        # Fails on the same terms as `just types`: a skip is a green gate that checked nothing.
        print(
            "no lua-language-server on PATH or in Zed's extensions. Install it: pacman -S lua-language-server",
            file=sys.stderr,
        )
        return 1

    paths = []
    for root in roots:
        for directory, _, names in os.walk(root):
            paths.extend(os.path.join(directory, name) for name in names if name.endswith(".lua"))
    paths.sort()
    if not paths:
        print("no Lua files under", " ".join(roots))
        return 0

    server = Server(binary, os.getcwd())
    try:
        unformatted = []
        for path in paths:
            with open(path, encoding="utf-8") as handle:
                source = handle.read()
            formatted = server.formatted(os.path.abspath(path), source)
            if formatted == source:
                continue
            unformatted.append(path)
            if not check_only:
                with open(path, "w", encoding="utf-8") as handle:
                    handle.write(formatted)
    finally:
        server.close()

    if check_only and unformatted:
        for path in unformatted:
            print(f"{path}: not formatted")
        print(f"\n{len(unformatted)} of {len(paths)} Lua files need formatting. Run `just fmt`.")
        return 1
    verb = "need formatting" if check_only else "reformatted"
    print(f"lua: {len(paths)} files checked, {len(unformatted)} {verb}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
