#!/usr/bin/env python3
"""A minimal kitty-graphics terminal for exercising terminal-browser headlessly.

Usage: kitty_term.py <report.json> <seconds|0> -- <command...>

Runs the command on a pseudo-terminal and answers the queries a
kitty-graphics terminal answers: the graphics probe (`ESC _ G ... a=q`),
primary device attributes (`ESC [ c`), window size in pixels (`ESC [ 14 t`)
and cell size in pixels (`ESC [ 16 t`). It draws nothing; it counts the image
transmissions the program sends and writes a JSON report on exit:

  {"graphics_queries": n, "frames": n, "exit": code}

It runs until the command exits, SIGTERM/SIGINT arrives, or the timeout (if
non-zero) passes; on SIGTERM it forwards SIGHUP to the child, like a
terminal closing.
"""
import json
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import fcntl
import time

COLS, ROWS, CELL_W, CELL_H = 160, 48, 10, 20


def main() -> int:
    report_path = sys.argv[1]
    seconds = float(sys.argv[2])
    command = sys.argv[sys.argv.index("--") + 1:]
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)
    fcntl.ioctl(fd, termios.TIOCSWINSZ,
                struct.pack("HHHH", ROWS, COLS, COLS * CELL_W, ROWS * CELL_H))
    stats = {"graphics_queries": 0, "frames": 0, "exit": None}
    stop = {"now": False}

    def on_signal(_signum, _frame):
        stop["now"] = True

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    started = time.time()
    buffer = b""
    while True:
        if stop["now"] or (seconds and time.time() - started > seconds):
            try:
                os.kill(pid, signal.SIGHUP)
            except ProcessLookupError:
                pass
            stop["now"] = False
            seconds = 0
            deadline = time.time() + 20
            while time.time() < deadline:
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    stats["exit"] = os.waitstatus_to_exitcode(status)
                    break
                try:
                    os.read(fd, 65536)
                except OSError:
                    pass
                time.sleep(0.1)
            break
        ready, _, _ = select.select([fd], [], [], 0.2)
        if not ready:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                stats["exit"] = os.waitstatus_to_exitcode(status)
                break
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            done, status = os.waitpid(pid, 0)
            stats["exit"] = os.waitstatus_to_exitcode(status)
            break
        if not chunk:
            break
        buffer = (buffer + chunk)[-1_000_000:]
        replies = b""
        for match in re.finditer(rb"\x1b_G([^;\x1b]*)(?:;[^\x1b]*)?\x1b\\", buffer):
            keys = dict(part.split(b"=", 1) for part in match.group(1).split(b",") if b"=" in part)
            if keys.get(b"a") == b"q":
                stats["graphics_queries"] += 1
                replies += b"\x1b_Gi=" + keys.get(b"i", b"0") + b";OK\x1b\\"
            elif keys.get(b"a") in (b"T", b"t", b"f") or b"t" in keys:
                stats["frames"] += 1
        if b"\x1b[c" in buffer:
            replies += b"\x1b[?62;22;52c"
        if b"\x1b[14t" in buffer:
            replies += b"\x1b[4;%d;%dt" % (ROWS * CELL_H, COLS * CELL_W)
        if b"\x1b[16t" in buffer:
            replies += b"\x1b[6;%d;%dt" % (CELL_H, CELL_W)
        if b"\x1b[18t" in buffer:
            replies += b"\x1b[8;%d;%dt" % (ROWS, COLS)
        # Drop everything already answered; keep a possibly split tail.
        tail = buffer.rfind(b"\x1b")
        buffer = buffer[tail:] if tail >= 0 and len(buffer) - tail < 64 else b""
        if replies:
            os.write(fd, replies)
    with open(report_path, "w", encoding="utf-8") as handle:
        json.dump(stats, handle)
    return 0


if __name__ == "__main__":
    sys.exit(main())
