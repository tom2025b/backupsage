#!/usr/bin/python3
"""Fake Borg/helper for process plumbing ONLY; no immutability proof.

All control, PID and synthetic output files live in the test's writable temp
directory reached through FD 9. Never used by the production runtime.
"""
import json
import os
import signal
import sys
import time

root = "/proc/self/fd/9"
with open(root + "/scenario") as f:
    scenario = f.read()


def record():
    fd = os.open(root + "/pids", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.write(fd, (str(os.getpid()) + "\n").encode())
    os.close(fd)


record()
if scenario == "hygiene":
    try:
        fd = os.open("/dev/tty", os.O_RDONLY | os.O_NONBLOCK)
        os.close(fd)
        tty = True
    except OSError:
        tty = False
    fds = []
    for n in range(3, 256):
        try:
            os.fstat(n)
            fds.append(n)
        except OSError:
            pass
    print(json.dumps({"stdin": os.read(0, 1).decode(), "tty": tty,
                      "fds": fds, "env": dict(os.environ)}))
elif scenario in ("timeout", "cancel", "early_exit", "parser_failure", "panic"):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    child = os.fork()
    if child == 0:
        record()
        grandchild = os.fork()
        if grandchild == 0:
            record()
        while True:
            time.sleep(1)
    # Barrier: consumers and cancellation tests wait for all three PID records.
    while True:
        with open(root + "/pids") as f:
            if len(f.readlines()) == 3:
                break
        time.sleep(0.005)
    if scenario == "early_exit":
        os._exit(17)
    if scenario in ("parser_failure", "panic"):
        os.write(1, b"synthetic parser input")
    while True:
        time.sleep(1)
elif scenario == "stderr":
    for _ in range(512):
        os.write(2, b"SENTINEL_PRIVATE_DIAGNOSTIC" * 512)
    os.write(1, b"healthy stdout")
elif scenario == "nonzero":
    os.write(2, b"SENTINEL_PRIVATE_DIAGNOSTIC")
    os._exit(23)
elif scenario in ("stream", "stdout_limit"):
    for _ in range(512):
        os.write(1, b"x" * 65536)
else:
    os._exit(24)
