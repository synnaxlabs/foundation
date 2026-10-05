"""Replication trade-study model for Foundation (research fork r13).

A Monte Carlo model, not a protocol simulator. Each alternative is reduced to the rules
that decide (1) when a frame is confirmed to its writer, (2) when complete readers may
see it, (3) which copies hold it at the moment a failure starts, and (4) what the
takeover does with copies that disagree. Random inputs: fsync latency, network RTT, and
the phase of the failure within the group-commit cycle.

All alternatives share one detection time (the lease or election timeout) so that the
comparison isolates protocol structure, not vendor defaults.

Run: python3 model.py  (prints markdown tables)
"""

import math
import random
import statistics

random.setstate(random.Random(13).getstate())  # fixed start state, replayable
N = 20000  # Monte Carlo draws per cell


def lognormal(median, p99):
    sigma = math.log(p99 / median) / 2.326
    mu = math.log(median)
    return lambda: random.lognormvariate(mu, sigma)


# Disk profiles: fsync (or full sync) latency in ms.
DISKS = {
    # Assumption: datacenter NVMe with power-loss protection.
    "nvme-plp": lognormal(0.1, 0.5),
    # Measured on this machine: F_FULLFSYNC p50 4.0 ms, p99 4.2-6.0 ms (fsync_bench.py).
    "laptop-ssd": lognormal(4.0, 6.0),
    # Single source (wiki.kewl.org rpi4 fio): Pi 4 SD card fsync ~240-260 ms, max ~1.9 s.
    "pi4-sd": lognormal(250.0, 1000.0),
}
# Network profiles: round-trip time in ms.
NETS = {
    "lan": lognormal(0.2, 1.0),  # assumption: switched site LAN
    "starlink": lognormal(45.0, 150.0),  # r4: median 40-50 ms, outliers > 100 ms
}
G = 2.0  # group commit interval, ms (B1 "every few ms")
D = 3000.0  # detection: lease or election timeout, ms (same for every alternative)
PROMOTE = 100.0  # voters commit + standby open + writers re-route, ms (estimate)
T_LAG = 1000.0  # ISR: lag after which the home asks voters to drop the standby, ms
T_SYNC = 10000.0  # semi-sync fallback timeout, ms (MySQL default)
WRITER_HOLD = 10000.0  # remote writer keeps up to this much unconfirmed data, ms
RETURN = 60000.0  # time until a crashed node returns, ms


def durable(fs):
    """Time from arrival to durable on one node with group commit: wait for the next
    commit to start (cycle = max(G, previous sync)), then one sync."""
    cycle = max(G, fs())
    return random.uniform(0, cycle) + fs()


def kth(xs, k):
    return sorted(xs)[k - 1]


def steady(disk, net, standby_net=None):
    """Confirmation and complete-reader latency per alternative, co-located writer."""
    fs, rt = DISKS[disk], NETS[net]
    srt = NETS[standby_net or net]
    out = {k: ([], []) for k in ALTS}
    for _ in range(N):
        dh = durable(fs)
        ds1, ds2 = durable(fs), durable(fs)
        r1, r2, r3, r4 = srt(), srt(), srt(), srt()
        # async after durable: standby receives after the home's sync
        repl_async = dh + r1 / 2 + ds1 + r1 / 2
        # parallel: standby receives on arrival
        par1 = r1 / 2 + ds1 + r1 / 2
        par2 = r2 / 2 + ds2 + r2 / 2
        lat = {
            "A1 async reader": (dh, dh),
            "A1R async + watermark": (repl_async, dh),
            "A2 Raft per index (3)": (max(dh, min(par1, par2)), max(dh, min(par1, par2))),
            "A3 ISR acks=all": (max(dh, par1), max(dh, par1)),
            "A3U acks=1 unclean": (dh, dh),
            "A4 source fan-out": (max(dh, par1), dh),
            "A5 chain (3)": (
                max(dh, r1 / 2 + ds1, r1 / 2 + r3 / 2 + ds2) + r4 / 2,
                max(dh, r1 / 2 + ds1, r1 / 2 + r3 / 2 + ds2) + r4 / 2,
            ),
            "A6 sync PB": (max(dh, par1), max(dh, par1)),
            "A7 leaderless W2/N3": (kth([dh, par1, par2], 2), dh),
        }
        for k, (c, r) in lat.items():
            out[k][0].append(c)
            out[k][1].append(r)
    return {k: (q(c, 0.5), q(c, 0.99), q(r, 0.99)) for k, (c, r) in out.items()}


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]


ALTS = [
    "A1 async reader",
    "A1R async + watermark",
    "A2 Raft per index (3)",
    "A3 ISR acks=all",
    "A3U acks=1 unclean",
    "A4 source fan-out",
    "A5 chain (3)",
    "A6 sync PB",
    "A7 leaderless W2/N3",
]


def failure(alt, scen, writer, disk="nvme-plp", net="lan"):
    """One draw. Returns (lost_confirmed, lost_total, delayed, outage).

    Data amounts are ms of data at the index's rate; outage is ms with no node accepting
    live writes. writer: 'local' (connector on the home node, dies with it) or 'remote'
    (survives, keeps unconfirmed frames up to WRITER_HOLD and resends them as backfill).
    "Confirmed" means confirmed at the strongest level the writer waits for.

    Every alternative except A3U re-merges a returning old home's durable suffix as
    deduplicated backfill (B7). Kafka and Raft as shipped truncate that suffix instead;
    A3U stands for that behavior.
    """
    fs, rt = DISKS[disk], NETS[net]
    f = fs()
    w_u = f + random.uniform(0, max(G, f))  # unsynced at the home when it fails
    eps = rt() / 2  # accepted, not yet on the wire to a replica
    returns = scen == "crash"
    lose_writer = writer == "local"
    lag = {"lag": 2000.0, "standby-partition": 20000.0}.get(scen, 0.0)

    if scen == "voter-partition":
        # Home alive, reaches the standby; only the voters are cut off.
        if alt in ("A2 Raft per index (3)", "A4 source fan-out", "A7 leaderless W2/N3"):
            return 0.0, 0.0, 0.0, 0.0  # no external voters decide these
        gap = 2 * 1.0 + PROMOTE  # home fences at lease end - e; standby promoted after + e
        if alt == "A3U acks=1 unclean":
            return 0.0, eps, 0.0, gap  # durable unsent suffix truncated
        return 0.0, 0.0, eps, gap  # suffix returns as backfill

    if scen == "both-crash":
        if alt in ("A2 Raft per index (3)", "A7 leaderless W2/N3"):
            lost = eps if lose_writer else 0.0  # third replica holds the rest
        else:
            lost = w_u if lose_writer else 0.0
        return 0.0, lost, 0.0, RETURN

    asynchronous = alt in ("A1 async reader", "A1R async + watermark", "A3U acks=1 unclean")
    slow_member_blocks = alt in ("A5 chain (3)",) and scen == "lag"
    if asynchronous:
        missing = w_u + eps + lag
        unsynced = w_u
        confirmed_missing = 0.0 if alt == "A1R async + watermark" else eps + lag
    elif alt == "A2 Raft per index (3)":
        missing, unsynced, confirmed_missing = eps, eps, 0.0  # second follower is current
    elif alt == "A3 ISR acks=all":
        missing, unsynced = eps + lag, eps
        confirmed_missing = max(0.0, lag - T_LAG)  # voters dropped the standby
    elif alt == "A6 sync PB":
        missing, unsynced = eps + lag, eps
        confirmed_missing = max(0.0, lag - T_SYNC)  # silent fallback to async
    elif alt == "A5 chain (3)":
        missing = eps + (lag if slow_member_blocks else 0.0)  # master re-links a cut link
        unsynced, confirmed_missing = eps, 0.0
    elif alt == "A4 source fan-out":
        # The writer queues per member; a local writer's queue dies with the home node.
        missing = eps + lag if lose_writer else 0.0
        unsynced, confirmed_missing = eps, 0.0
    elif alt == "A7 leaderless W2/N3":
        missing, unsynced, confirmed_missing = eps, eps, 0.0
    else:
        raise KeyError(alt)

    # A surviving writer holds the newest unconfirmed frames.
    held = 0.0 if lose_writer else min(missing - confirmed_missing, WRITER_HOLD)
    unrecovered = max(0.0, missing - held)
    # The oldest unrecovered frames are durable on the old home's disk.
    on_old_disk = min(unrecovered, max(0.0, missing - unsynced))
    if returns and alt != "A3U acks=1 unclean":
        delayed, lost = on_old_disk, unrecovered - on_old_disk
    else:
        delayed, lost = 0.0, unrecovered
    # Confirmed frames are the oldest missing ones. A returning old home re-merges
    # them (except A3U, which truncates), so only the unsynced newest frames are lost.
    if returns and alt != "A3U acks=1 unclean":
        lost_confirmed = 0.0
    else:
        lost_confirmed = min(confirmed_missing, lost)

    outage = D + PROMOTE
    if alt in ("A4 source fan-out", "A7 leaderless W2/N3") and not lose_writer:
        outage = 0.0  # the writer keeps writing to the other members
    if alt == "A3 ISR acks=all" and lag > T_LAG and not returns:
        outage = math.inf  # in-sync set is empty; waits for an operator (unclean off)
    return lost_confirmed, lost, delayed, outage


SCENARIOS = [
    ("crash", "home crash, disk survives, back in 60 s"),
    ("disk-loss", "home node destroyed (disk lost)"),
    ("voter-partition", "home cut from voters 60 s, reaches standby"),
    ("standby-partition", "home cut from standby 20 s, then destroyed"),
    ("lag", "standby 2 s behind (slow disk), then home destroyed"),
    ("both-crash", "home and standby crash, back in 60 s"),
]


def failure_table(writer, disk="nvme-plp"):
    rows = []
    for alt in ALTS:
        cells = []
        for scen, _ in SCENARIOS:
            draws = [failure(alt, scen, writer, disk) for _ in range(2000)]
            lc = q([d[0] for d in draws], 0.99)
            lt = q([d[1] for d in draws], 0.99)
            dl = q([d[2] for d in draws], 0.99)
            ou = q([d[3] for d in draws], 0.5)
            cells.append(fmt_cell(lc, lt, dl, ou))
        rows.append((alt, cells))
    return rows


def fmt_ms(x):
    if math.isinf(x):
        return "inf"
    if x >= 1000:
        return f"{x / 1000:.1f}s"
    if x >= 10:
        return f"{x:.0f}ms"
    return f"{x:.1f}ms"


def fmt_cell(lc, lt, dl, ou):
    return f"C {fmt_ms(lc)} / L {fmt_ms(lt)} / D {fmt_ms(dl)} / O {fmt_ms(ou)}"


def flapping(mean_up, mean_down, lease, failback, seconds=600, step=10):
    """Home-to-voters link flaps. Lease renewed every lease/3 while the link is up.
    Returns (failovers, ms without an accepting home)."""
    t, up, next_flip = 0, True, random.expovariate(1 / mean_up)
    last_renew = 0.0
    home_is_primary = True
    failovers = 0
    outage = 0.0
    fenced_until = -1.0
    while t < seconds * 1000:
        if t >= next_flip:
            up = not up
            next_flip = t + random.expovariate(1 / (mean_up if up else mean_down))
        if home_is_primary:
            if up and t - last_renew >= lease / 3:
                last_renew = t
            if t - last_renew > lease:
                failovers += 1
                home_is_primary = False
                outage += 2 * 1.0 + PROMOTE
                last_renew = t
        else:
            if failback and up:
                # automatic failback to the preferred node once it renews again
                failovers += 1
                home_is_primary = True
                outage += 2 * 1.0 + PROMOTE
                last_renew = t
        t += step
    return failovers, outage


def isr_churn(mean_up, mean_down, t_lag, seconds=600, step=10):
    """Home-to-standby link flaps. ISR: shrink after t_lag of no progress, expand
    when caught up. Returns (ISR changes, ms of stalled confirmations)."""
    t, up, next_flip = 0, True, random.expovariate(1 / mean_up)
    in_sync = True
    stalled_since = None
    changes = 0
    stall = 0.0
    while t < seconds * 1000:
        if t >= next_flip:
            up = not up
            next_flip = t + random.expovariate(1 / (mean_up if up else mean_down))
        if in_sync:
            if not up:
                stalled_since = t if stalled_since is None else stalled_since
                stall += step
                if t - stalled_since >= t_lag:
                    in_sync = False
                    changes += 1
            else:
                stalled_since = None
        else:
            if up:
                in_sync = True  # catch-up time ignored (LAN)
                changes += 1
                stalled_since = None
        t += step
    return changes, stall


def main():
    print("## Steady state: confirmation p50 / p99 and complete-reader p99 (ms)\n")
    for disk, net, snet in (
        ("nvme-plp", "lan", None),
        ("laptop-ssd", "lan", None),
        ("pi4-sd", "lan", None),
        ("nvme-plp", "lan", "starlink"),
    ):
        res = steady(disk, net, snet)
        label = f"{disk}, standby over {snet or net}"
        print(f"### {label}\n")
        print("| Alternative | confirm p50 | confirm p99 | complete reader p99 |")
        print("|---|---|---|---|")
        for k in ALTS:
            c50, c99, r99 = res[k]
            print(f"| {k} | {c50:.2f} | {c99:.2f} | {r99:.2f} |")
        print()

    for writer, disk in (("local", "nvme-plp"), ("remote", "nvme-plp"), ("local", "pi4-sd")):
        print(f"## Failures, writer {writer} (p99 data, p50 outage; {disk}, LAN)\n")
        print("C = confirmed and lost, L = lost for good, D = delayed until the old home")
        print("returns, O = time with no accepting home.\n")
        print("| Alternative | " + " | ".join(s for s, _ in SCENARIOS) + " |")
        print("|---|" + "---|" * len(SCENARIOS))
        for alt, cells in failure_table(writer, disk):
            print(f"| {alt} | " + " | ".join(cells) + " |")
        print()
    print("Scenario key:")
    for s, d in SCENARIOS:
        print(f"- {s}: {d}")
    print()

    print("## Flapping home-to-voters link, 600 s (mean of 50 runs)\n")
    print("| up / down mean (s) | lease (s) | failovers, no failback | failovers, auto failback |")
    print("|---|---|---|---|")
    for up, down in ((20, 0.5), (3, 2), (15, 1.5)):
        for lease in (1000, 3000, 10000):
            a = [flapping(up * 1000, down * 1000, lease, False)[0] for _ in range(50)]
            b = [flapping(up * 1000, down * 1000, lease, True)[0] for _ in range(50)]
            print(
                f"| {up} / {down} | {lease / 1000:.0f} | {statistics.mean(a):.2f} | "
                f"{statistics.mean(b):.1f} |"
            )
    print()
    print("## Flapping home-to-standby link, ISR churn, 600 s (mean of 50 runs)\n")
    print("| up / down mean (s) | T_lag (s) | ISR changes (voter commits) | confirmations stalled (s) |")
    print("|---|---|---|---|")
    for up, down in ((20, 0.5), (3, 2), (15, 1.5)):
        for t_lag in (1000, 10000):
            r = [isr_churn(up * 1000, down * 1000, t_lag) for _ in range(50)]
            print(
                f"| {up} / {down} | {t_lag / 1000:.0f} | "
                f"{statistics.mean(x[0] for x in r):.1f} | "
                f"{statistics.mean(x[1] for x in r) / 1000:.1f} |"
            )


if __name__ == "__main__":
    main()
