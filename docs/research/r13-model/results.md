## Steady state: confirmation p50 / p99 and complete-reader p99 (ms)

### nvme-plp, standby over lan

| Alternative | confirm p50 | confirm p99 | complete reader p99 |
|---|---|---|---|
| A1 async reader | 1.14 | 2.19 | 2.19 |
| A1R async + watermark | 2.51 | 4.33 | 2.19 |
| A2 Raft per index (3) | 1.42 | 2.25 | 2.25 |
| A3 ISR acks=all | 1.68 | 2.69 | 2.69 |
| A3U acks=1 unclean | 1.14 | 2.19 | 2.19 |
| A4 source fan-out | 1.68 | 2.69 | 2.19 |
| A5 chain (3) | 1.96 | 2.78 | 2.78 |
| A6 sync PB | 1.68 | 2.69 | 2.69 |
| A7 leaderless W2/N3 | 1.29 | 2.20 | 2.19 |

### laptop-ssd, standby over lan

| Alternative | confirm p50 | confirm p99 | complete reader p99 |
|---|---|---|---|
| A1 async reader | 6.02 | 9.51 | 9.51 |
| A1R async + watermark | 12.35 | 17.38 | 9.51 |
| A2 Raft per index (3) | 6.51 | 9.52 | 9.52 |
| A3 ISR acks=all | 7.02 | 10.04 | 10.04 |
| A3U acks=1 unclean | 6.02 | 9.51 | 9.51 |
| A4 source fan-out | 7.02 | 10.04 | 9.51 |
| A5 chain (3) | 7.56 | 10.40 | 10.40 |
| A6 sync PB | 7.02 | 10.04 | 10.04 |
| A7 leaderless W2/N3 | 6.20 | 8.62 | 9.51 |

### pi4-sd, standby over lan

| Alternative | confirm p50 | confirm p99 | complete reader p99 |
|---|---|---|---|
| A1 async reader | 396.86 | 1282.96 | 1282.96 |
| A1R async + watermark | 839.81 | 1969.39 | 1282.96 |
| A2 Raft per index (3) | 447.89 | 1288.29 | 1288.29 |
| A3 ISR acks=all | 521.37 | 1444.58 | 1444.58 |
| A3U acks=1 unclean | 396.86 | 1282.96 | 1282.96 |
| A4 source fan-out | 521.37 | 1444.58 | 1282.96 |
| A5 chain (3) | 597.41 | 1551.64 | 1551.64 |
| A6 sync PB | 521.37 | 1444.58 | 1444.58 |
| A7 leaderless W2/N3 | 395.74 | 862.72 | 1282.96 |

### nvme-plp, standby over starlink

| Alternative | confirm p50 | confirm p99 | complete reader p99 |
|---|---|---|---|
| A1 async reader | 1.12 | 2.18 | 2.18 |
| A1R async + watermark | 46.97 | 152.98 | 2.18 |
| A2 Raft per index (3) | 35.05 | 89.41 | 89.41 |
| A3 ISR acks=all | 45.86 | 151.68 | 151.68 |
| A3U acks=1 unclean | 1.12 | 2.18 | 2.18 |
| A4 source fan-out | 45.86 | 151.68 | 2.18 |
| A5 chain (3) | 74.10 | 152.08 | 152.08 |
| A6 sync PB | 45.86 | 151.68 | 151.68 |
| A7 leaderless W2/N3 | 35.05 | 89.41 | 2.18 |

## Failures, writer local (p99 data, p50 outage; nvme-plp, LAN)

C = confirmed and lost, L = lost for good, D = delayed until the old home
returns, O = time with no accepting home.

| Alternative | crash | disk-loss | voter-partition | standby-partition | lag | both-crash |
|---|---|---|---|---|---|---|
| A1 async reader | C 0.0ms / L 2.2ms / D 0.5ms / O 3.1s | C 0.5ms / L 2.4ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 20.0s / L 20.0s / D 0.0ms / O 3.1s | C 2.0s / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A1R async + watermark | C 0.0ms / L 2.2ms / D 0.5ms / O 3.1s | C 0.0ms / L 2.4ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 20.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A2 Raft per index (3) | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 60.0s |
| A3 ISR acks=all | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 19.0s / L 20.0s / D 0.0ms / O inf | C 1.0s / L 2.0s / D 0.0ms / O inf | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A3U acks=1 unclean | C 0.5ms / L 2.3ms / D 0.0ms / O 3.1s | C 0.5ms / L 2.4ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 102ms | C 20.0s / L 20.0s / D 0.0ms / O 3.1s | C 2.0s / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A4 source fan-out | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 20.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A5 chain (3) | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A6 sync PB | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 10.0s / L 20.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.2ms / D 0.0ms / O 60.0s |
| A7 leaderless W2/N3 | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.6ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 60.0s |

## Failures, writer remote (p99 data, p50 outage; nvme-plp, LAN)

C = confirmed and lost, L = lost for good, D = delayed until the old home
returns, O = time with no accepting home.

| Alternative | crash | disk-loss | voter-partition | standby-partition | lag | both-crash |
|---|---|---|---|---|---|---|
| A1 async reader | C 0.0ms / L 0.0ms / D 0.5ms / O 3.1s | C 0.5ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.4ms / O 102ms | C 20.0s / L 20.0s / D 0.0ms / O 3.1s | C 2.0s / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A1R async + watermark | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 10.0s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A2 Raft per index (3) | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A3 ISR acks=all | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 19.0s / L 19.0s / D 0.0ms / O inf | C 1.0s / L 1.0s / D 0.0ms / O inf | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A3U acks=1 unclean | C 0.5ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.5ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.6ms / D 0.0ms / O 102ms | C 20.0s / L 20.0s / D 0.0ms / O 3.1s | C 2.0s / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A4 source fan-out | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A5 chain (3) | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A6 sync PB | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 10.0s / L 10.0s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |
| A7 leaderless W2/N3 | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.0ms / D 0.0ms / O 60.0s |

## Failures, writer local (p99 data, p50 outage; pi4-sd, LAN)

C = confirmed and lost, L = lost for good, D = delayed until the old home
returns, O = time with no accepting home.

| Alternative | crash | disk-loss | voter-partition | standby-partition | lag | both-crash |
|---|---|---|---|---|---|---|
| A1 async reader | C 0.0ms / L 1.6s / D 0.5ms / O 3.1s | C 0.5ms / L 1.6s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 20.0s / L 21.7s / D 0.0ms / O 3.1s | C 2.0s / L 3.6s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A1R async + watermark | C 0.0ms / L 1.6s / D 0.5ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 21.5s / D 0.0ms / O 3.1s | C 0.0ms / L 3.6s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A2 Raft per index (3) | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.6ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 60.0s |
| A3 ISR acks=all | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 19.0s / L 20.0s / D 0.0ms / O inf | C 1.0s / L 2.0s / D 0.0ms / O inf | C 0.0ms / L 1.8s / D 0.0ms / O 60.0s |
| A3U acks=1 unclean | C 0.5ms / L 1.5s / D 0.0ms / O 3.1s | C 0.5ms / L 1.5s / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 102ms | C 20.0s / L 21.6s / D 0.0ms / O 3.1s | C 2.0s / L 3.5s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A4 source fan-out | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 20.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A5 chain (3) | C 0.0ms / L 0.6ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A6 sync PB | C 0.0ms / L 0.6ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.5ms / O 102ms | C 10.0s / L 20.0s / D 0.0ms / O 3.1s | C 0.0ms / L 2.0s / D 0.0ms / O 3.1s | C 0.0ms / L 1.6s / D 0.0ms / O 60.0s |
| A7 leaderless W2/N3 | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.6ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.0ms / D 0.0ms / O 0.0ms | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 3.1s | C 0.0ms / L 0.5ms / D 0.0ms / O 60.0s |

Scenario key:
- crash: home crash, disk survives, back in 60 s
- disk-loss: home node destroyed (disk lost)
- voter-partition: home cut from voters 60 s, reaches standby
- standby-partition: home cut from standby 20 s, then destroyed
- lag: standby 2 s behind (slow disk), then home destroyed
- both-crash: home and standby crash, back in 60 s

## Flapping home-to-voters link, 600 s (mean of 50 runs)

| up / down mean (s) | lease (s) | failovers, no failback | failovers, auto failback |
|---|---|---|---|
| 20 / 0.5 | 1 | 1.00 | 10.0 |
| 20 / 0.5 | 3 | 0.24 | 0.3 |
| 20 / 0.5 | 10 | 0.00 | 0.0 |
| 3 / 2 | 1 | 1.00 | 161.1 |
| 3 / 2 | 3 | 1.00 | 67.8 |
| 3 / 2 | 10 | 0.86 | 4.7 |
| 15 / 1.5 | 1 | 1.00 | 42.3 |
| 15 / 1.5 | 3 | 1.00 | 13.1 |
| 15 / 1.5 | 10 | 0.14 | 0.3 |

## Flapping home-to-standby link, ISR churn, 600 s (mean of 50 runs)

| up / down mean (s) | T_lag (s) | ISR changes (voter commits) | confirmations stalled (s) |
|---|---|---|---|
| 20 / 0.5 | 1 | 8.2 | 12.8 |
| 20 / 0.5 | 10 | 0.0 | 14.7 |
| 3 / 2 | 1 | 144.0 | 96.2 |
| 3 / 2 | 10 | 1.6 | 239.0 |
| 15 / 1.5 | 1 | 36.8 | 26.0 |
| 15 / 1.5 | 10 | 0.0 | 55.5 |
