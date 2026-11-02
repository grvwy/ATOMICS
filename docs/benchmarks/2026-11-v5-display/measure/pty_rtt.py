"""Keystroke echo round trip through a PTY: write one byte, time until it is echoed back. argv: label n cmd..."""
import os, pty, select, sys, time, statistics
label, n, cmd = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(cmd[0], cmd)
def drain(timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: os.read(fd, 65536)
            except OSError: return
drain(3.0)
samples = []
for i in range(n):
    ch = bytes([97 + i % 26])
    t0 = time.perf_counter()
    os.write(fd, ch)
    buf = b""
    while ch not in buf:
        r, _, _ = select.select([fd], [], [], 2.0)
        if not r: break
        buf += os.read(fd, 4096)
    samples.append((time.perf_counter() - t0) * 1000)
    if i % 40 == 39: os.write(fd, b"\r")
    time.sleep(0.02)
    drain(0.0)
os.kill(pid, 9)
samples.sort()
q = lambda p: samples[min(len(samples) - 1, round((len(samples) - 1) * p))]
print(f"{label}: n={len(samples)} p50={q(.5):.2f} p90={q(.9):.2f} p99={q(.99):.2f} max={samples[-1]:.2f} ms")
