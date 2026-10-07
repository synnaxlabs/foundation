# Prints the CPU of the core whose two threads were least busy over 2 s, with the
# busy share of each thread and of the whole host. Tries up to 6 times for a core
# with both threads under 35% busy, then takes the best one seen.
import time
def snap():
    d = {}
    for l in open('/proc/stat'):
        if l.startswith('cpu') and l[3].isdigit():
            p = l.split(); v = list(map(int, p[1:9])); d[int(p[0][3:])] = (sum(v), v[3] + v[4])
    return d
best = None
for attempt in range(6):
    a = snap(); time.sleep(2); b = snap()
    busy = {c: 100 * (1 - (b[c][1] - a[c][1]) / max(1, b[c][0] - a[c][0])) for c in a}
    c = min(range(1, 32), key=lambda c: busy[c] + busy[c + 32])
    host = sum(busy.values()) / len(busy)
    got = (max(busy[c], busy[c + 32]), c, busy[c], busy[c + 32], host)
    if best is None or got < best:
        best = got
    if got[0] < 35:
        break
_, c, b1, b2, host = best
print(c, f'{b1:.0f}', f'{b2:.0f}', f'{host:.0f}')
