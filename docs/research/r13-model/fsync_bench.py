# Measures group-commit-sized append + full sync latency on this machine.
import os, fcntl, time, statistics, sys
path = "fsync_probe.bin"
size = int(sys.argv[1]) if len(sys.argv) > 1 else 64 * 1024
n = int(sys.argv[2]) if len(sys.argv) > 2 else 400
buf = os.urandom(size)
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
# preallocate and sync once so the loop never extends the file (r2 ring design)
os.write(fd, b"\0" * (size * n)); fcntl.fcntl(fd, fcntl.F_FULLFSYNC)
lat_full, lat_plain = [], []
for mode in ("full", "plain"):
    os.lseek(fd, 0, 0)
    for i in range(n):
        os.write(fd, buf)
        t = time.perf_counter()
        if mode == "full":
            fcntl.fcntl(fd, fcntl.F_FULLFSYNC)
        else:
            os.fsync(fd)
        (lat_full if mode == "full" else lat_plain).append((time.perf_counter() - t) * 1e3)
os.close(fd); os.remove(path)
def q(xs, p): xs = sorted(xs); return xs[min(len(xs) - 1, int(p * len(xs)))]
for name, xs in (("F_FULLFSYNC", lat_full), ("fsync (no flush)", lat_plain)):
    print(f"{name:18s} write={size//1024}KiB n={n} p50={q(xs,.5):.3f} ms p90={q(xs,.9):.3f} p99={q(xs,.99):.3f} max={max(xs):.3f}")
