#!/usr/bin/env python3
"""Drive the llamastash TUI non-interactively in a pty and dump emulated screens.

Agent/e2e helper for verifying TUI behavior a golden snapshot can't reach:
real daemon state, key-driven flows (filter → stage picker → launch), and
panel content under live data. Spawns the working-tree binary in a pty,
feeds it a scripted key sequence, and prints the pyte-emulated screen after
each step so the output is plain text (no ANSI parsing needed).

Usage:
    python3 scripts/tui/tui_drive.py '<script-json>' [--bin PATH] [--size COLSxROWS]

The script is a JSON array of [keys, wait_seconds, label] steps:
    python3 scripts/tui/tui_drive.py '[["", 4, "boot"],
                                      ["/gemma|<enter>", 2, "staged"],
                                      ["<enter>", 3, "launched"]]'

Keys are sent as literal characters; `|` separates tokens inside one step and
these named tokens map to escape sequences: <down> <up> <left> <right>
<enter> <esc> <tab>, plus <ctrl-x> for any control chord (e.g. <ctrl-p>)
and <alt-x> for any Alt chord (e.g. <alt-l>).
After each step's keys, the driver pumps pty output for
`wait_seconds`, then prints the screen under a `===== SCREEN: <label> =====`
header. `q` is sent at the end so the TUI exits cleanly.

Environment: the child inherits this process's env verbatim — set
LLAMASTASH_STATE_DIR / LLAMASTASH_CONFIG_DIR / LLAMASTASH_CACHE_DIR / HF_HOME
to aim it at an isolated daemon (see AGENTS.md §Dev commands). TERM is forced
to xterm-256color.

Requires `pyte` (pip install pyte — not in scripts/requirements.txt because
only this helper needs it; a venv is fine).
"""

import argparse
import fcntl
import json
import os
import pty
import select
import struct
import sys
import termios
import time

KEYMAP = {
    "<down>": "\x1b[B",
    "<up>": "\x1b[A",
    "<right>": "\x1b[C",
    "<left>": "\x1b[D",
    "<enter>": "\r",
    "<esc>": "\x1b",
    "<tab>": "\t",
}


def expand(token: str) -> str:
    """Map a token to the bytes to send. `<ctrl-x>` becomes the control
    byte for letter x (Ctrl+P -> 0x10); `<alt-x>` becomes ESC+x in one write
    (how terminals encode Alt+letter, so crossterm decodes it as Alt+x rather
    than a bare Esc); known tokens use KEYMAP; anything else is sent literally
    (so plain text types itself)."""
    if token.startswith("<ctrl-") and token.endswith(">") and len(token) == 8:
        return chr(ord(token[6].upper()) - 0x40)
    if token.startswith("<alt-") and token.endswith(">") and len(token) == 7:
        return "\x1b" + token[5]
    return KEYMAP.get(token, token)


def default_bin() -> str:
    # scripts/tui/tui_drive.py -> repo root is three levels up.
    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    return os.path.join(repo, "target", "debug", "llamastash")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("script", help="JSON array of [keys, wait_seconds, label] steps")
    ap.add_argument("--bin", default=default_bin(), help="llamastash binary (default: target/debug)")
    ap.add_argument("--size", default="160x45", help="terminal size COLSxROWS (default 160x45)")
    args = ap.parse_args()

    try:
        import pyte
    except ImportError:
        sys.exit("tui_drive: needs `pyte` (pip install pyte)")

    cols, rows = (int(v) for v in args.size.split("x"))
    steps = json.loads(args.script)
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    env = dict(os.environ, TERM="xterm-256color")

    pid, fd = pty.fork()
    if pid == 0:
        os.execve(args.bin, [args.bin], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(duration: float) -> None:
        end = time.time() + duration
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if fd in ready:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                stream.feed(data)

    for keys, wait, label in steps:
        for token in keys.split("|"):
            if not token:
                continue
            os.write(fd, expand(token).encode())
            pump(0.3)
        pump(wait)
        print(f"\n===== SCREEN: {label} =====")
        for line in screen.display:
            print(line.rstrip())

    os.write(fd, b"q")
    pump(1.0)
    try:
        os.kill(pid, 15)
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    main()
