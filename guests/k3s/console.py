#!/usr/bin/env python3
"""Runs a shell command in the guest over Hearth's serial console socket
(--serial unix:PATH) and prints its output; exits with its status.

Usage: console.py SOCKET COMMAND [TIMEOUT]

COMMAND goes on one line, between two printf's.
The guest must have a root shell on the console, as make-alpine-rootfs.sh
gives it. The command's output is told from the console's other output
(the echoed command line, what Hearth replays from before this client
connected, kernel messages) by markers that only the shell's printf
puts together. Until a shell answers, the command is sent again every
few seconds, so it can be given while the guest still boots. Exits 124
when TIMEOUT seconds [default: 60] pass without an answer."""

import os
import re
import socket
import sys
import time


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit(__doc__)
    path, command = sys.argv[1], sys.argv[2]
    timeout = float(sys.argv[3]) if len(sys.argv) == 4 else 60.0
    deadline = time.monotonic() + timeout

    tag = "%d_%d" % (os.getpid(), int(time.time()))

    def wrapped(name, cmd):
        """cmd between markers __HEARTH_<name>_<tag>__, the second one
        followed by cmd's status; the echoed line has them split."""
        return ("printf '%%s%%s\\n' __HEARTH_%s_ %s__; %s; "
                "printf '%%s%%s %%s\\n' __HEARTH_%s_END_ %s__ $?\n" % (name, tag, cmd, name, tag))

    def pattern(name):
        mark = "__HEARTH_%s_%s__" % (name, tag)
        end = "__HEARTH_%s_END_%s__" % (name, tag)
        regex = re.compile(re.escape(mark) + r"\r*\n(.*?)" + re.escape(end) + r" (\d+)\r*\n", re.S)
        return regex, end.encode()

    sock = None
    while sock is None:
        try:
            sock = socket.socket(socket.AF_UNIX)
            sock.connect(path)
        except OSError:
            sock = None
            if time.monotonic() > deadline:
                sys.exit("console.py: cannot connect to %s" % path)
            time.sleep(0.5)

    received = b""

    def wait_for(pat, resend=None):
        """Reads until the pattern matches what came since the call,
        sending resend every few seconds meanwhile; None at the deadline.
        The regex only runs once the end marker has come, so a long
        output costs one scan, and reading keeps up with Hearth."""
        nonlocal received
        regex, end = pat
        start = len(received)
        scanned = start
        end_seen = False
        next_send = 0.0
        while time.monotonic() < deadline:
            if resend and time.monotonic() >= next_send:
                sock.sendall(resend.encode())
                next_send = time.monotonic() + 5.0
            sock.settimeout(0.5)
            try:
                data = sock.recv(1 << 20)
                if not data:
                    sys.exit("console.py: Hearth closed the console")
                received += data
            except socket.timeout:
                pass
            if not end_seen:
                end_seen = received.find(end, max(start, scanned - len(end))) >= 0
                scanned = len(received)
                if not end_seen:
                    continue
            m = regex.search(received[start:].decode(errors="replace"))
            if m:
                return m
        return None

    # Until a shell answers, only the probe is sent again: the command
    # itself must run once.
    if not wait_for(pattern("READY"), resend=wrapped("READY", "true")):
        sys.exit(124)
    sock.sendall(wrapped("CMD", command).encode())
    m = wait_for(pattern("CMD"))
    if not m:
        sys.exit(124)
    sys.stdout.write(m.group(1).replace("\r", ""))
    sys.exit(int(m.group(2)))


if __name__ == "__main__":
    main()
