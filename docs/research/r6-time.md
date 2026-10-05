# Foundation research fork 6: time (C6)

Date: 2026-10-04. Scope: source accuracy, build vs adopt, error bounds, automatic source
detection, device clock re-anchoring. Locked context: C6 (mesh clock = local clock plus
measured offset with an interval bound; time source is a policy; no OS steering by
default; automation requirement), S6 (index points at an error channel; strictly
increasing timestamps), S8 (`<node>.clock.offset`, `.clock.error` channels), T1
(clock, network, disk injected).

Verification marks: [V2] = two or more independent sources agree. [V1] = one source
only. [U] = unverified or inferred.

---

## Q1. What accuracy can each source reach?

**Recommendation:** treat the published error bound as the product's accuracy, and set
per-class targets that the simulator and HITL rigs check, because real accuracy spans six
orders of magnitude (tens of ns to tens of ms) depending on the path.

| Source | Practical accuracy | Mark | Evidence |
|---|---|---|---|
| NTP on a LAN, software timestamps | tens of µs | [V2] | chrony docs via [Wikipedia: Chrony](https://en.wikipedia.org/wiki/Chrony); [Red Hat chrony guide](https://docs.redhat.com/en/documentation/Red_Hat_Enterprise_Linux/7/html/System_Administrators_Guide/ch-Configuring_NTP_Using_the_chrony_Suite.html) |
| NTP or PTP on a LAN, hardware timestamps on both ends | tens of ns (chrony example: +33 ns, sd 24 ns; ~70 ns from asymmetry) | [V2] | [chrony examples](https://chrony-project.org/examples.html); Wikipedia: Chrony |
| PTP, software timestamps | 10 to 100 µs | [V2] | [EndRun](https://endruntechnologies.com/node/249); Microsoft: Windows stack adds 30 to 200 µs ([ghacks summary of MS blog](https://www.ghacks.net/2018/07/20/windows-time-gets-accuracy-improvements-and-leap-second-support/), [ATIS slides](https://tam.atis.org/wp-content/uploads/2021/06/Recent_Improvements_to_High-Accuracy_Timekeeping_in_Microsoft_Windows-Microsoft.pdf)) |
| PTP, hardware timestamps, PTP-aware network | sub-µs; NI TSN chassis < 1 µs, tens of ns per bridge | [V2] | [NI TSN guide](https://www.ni.com/en/shop/data-acquisition/how-to-achieve-high-accuracy-measurements-with-ni-daqmx-based-ts.html); [SUSE PTP tuning](https://documentation.suse.com/en-us/sles/15-SP7/html/SLES-all/cha-tuning-ptp.html) |
| PTP hardware timestamps through ordinary switches | delay variation grows with load; Meinberg filtered 100 µs jitter down to ~5 µs | [V1] | [Meinberg blog](https://blog.meinbergglobal.com/2016/06/29/ptp-on-path-support-tests-part-ii) |
| NTP over the internet | ~1 to 10 ms typical; honest bound up to RTT/2 | [V2] | ntpd-rs README status sample (±4 to ±17 ms bounds per source, [README](https://github.com/pendulum-project/ntpd-rs)); [ClockBound formula](https://docs.amazonaws.cn/en_us/AWSEC2/latest/UserGuide/compare-timestamps-with-clockbound.html) |
| NTP over Starlink | one-way uplink 52 ± 14 ms, downlink 35 ± 11 ms; ~17 ms asymmetry gives ~8 ms bias; honest bound ~RTT/2 ≈ 40 to 45 ms; spikes at the 15 s reconfiguration | [V1] measurements, [V2] mechanism | [Garcia et al., "A Detailed Characterization of Starlink One-way Delay", LEO-NET 2025](https://dl.acm.org/doi/10.1145/3748749.3749090); [Mohan et al. WWW 2024](https://www.nitindermohan.com/documents/2024/pubs/starlinkWWW2024.pdf) |
| Starlink dish's own NTP server (GPS-derived, 192.168.100.1) | stratum 1 on the LAN; accuracy not measured | [V2] exists, [U] accuracy | [NTP Pool community](https://community.ntppool.org/t/starlink-enables-ntp-server-with-gps-time-for-local-use/3520); [OpenWrt forum](https://forum.openwrt.org/t/time-server-for-starlink-users/248510) |
| NTP over LTE | sd 2.7 to 7 ms; errors > 50 ms frequent in one study; wireless mean 31 ms, sd 47 ms in another | [V2] | [arXiv 2306.00633](https://arxiv.org/pdf/2306.00633); [Sommers IMC16](https://cs.colgate.edu/~jsommers/pubs/imc16.pdf) |
| GPS receiver PPS output | < 20 ns (u-blox M8T); 30 to 50 ns RMS older modules | [V2] | [GPS World u-blox](https://www.gpsworld.com/u-blox-gnss-timing-modules-designed-for-upgrades/); [time-nuts LEA-5T](https://www.febo.com/pipermail/time-nuts/2008-April/030925.html) |
| GPS PPS captured by a host | kernel GPIO PPS ~1 µs (RPi: 684 ns offset); native serial DCD a few µs; USB serial ~1 ms (USB 1) to a few µs (USB 3); NMEA without PPS ~100 µs to ms | [V2] | [Austin's Nerdy Things](https://austinsnerdythings.com/2021/04/19/microsecond-accurate-ntp-with-a-raspberry-pi-and-pps-gps/); [chrony FAQ commit](https://git.faraphel.fr/faraphel/chrony/commit/e2e07af8a45c23d2a1db1d750cb11e43e4fc270c); [NTP PPS docs](https://docs.ntpsec.org/latest/pps.html) |
| Windows W32Time | 1 ms best, only under the support boundary (Win10 1607 / Server 2016+, every hop high-accuracy, stratum-1 root, low asymmetry); 50 ms and 1 s tiers otherwise | [V2] | [MS support boundary](https://learn.microsoft.com/windows-server/networking/windows-time-service/support-boundary); [MS KB 939322](https://support.microsoft.com/kb/939322) |
| Windows PTP client (Server 2019+) | software timestamps only; partner measured 41 µs RMS on pre-release | [V1] | [ghacks](https://www.ghacks.net/2018/07/20/windows-time-gets-accuracy-improvements-and-leap-second-support/); [FSMLabs](https://fsmlabs.com/new-release-of-timekeeper-achieves-superior-benchmark-time-accuracy-on-microsoft-windows-using-server-2019-high-precision-clock-api/) |
| Windows hardware timestamping | API from build 20348 (`GetInterfaceActiveTimestampCapabilities`, `SIO_TIMESTAMPING`); driver support uneven (`ERROR_BAD_DRIVER` reports on Intel NICs) | [V2] | [MS packet timestamping](https://learn.microsoft.com/en-us/windows/win32/iphlp/packet-timestamping); [Intel community](https://community.intel.com/t5/Ethernet-Products/Hardware-Timestamp-on-windows/m-p/1441832) |
| macOS | `timed` is a simple SNTP client that steps with `settimeofday`; `ntp_adjtime` exists since 10.13; no PPS API, no user-space hardware timestamps | [V2] | [Apple forum](https://developer.apple.com/forums/thread/83240); [chrony macOS commit](https://git.faraphel.fr/faraphel/chrony/commit/ccb94ac5fbc84c04986eedda3b23196aaa64e4fb) |
| DAQ and device oscillators | NI DAQ timebase ~50 ppm (~5 s/day); LabJack T7 20 ppm (72 ms/h); host ~100 ppm; device vs host up to 120 ppm (10.4 s/day) | [V2] | [NI forum](https://forums.ni.com/t5/LabVIEW/Can-I-rely-on-DAQmx-timing/m-p/4286414); [LabJack stream timing](https://support.labjack.com/docs/3-2-1-stream-timing-t-series-datasheet) |

Rejected: promising one accuracy number. A Starlink site without a local source has an
honest bound near 40 ms, and a PTP lab has 50 ns. A single claim would be false for one
of them.

Risks: Starlink dish NTP accuracy is unmeasured; NMEA-only GPS on Windows and macOS is
ms class because neither OS has a PPS API.

**Decision for the user:** adopt per-class accuracy targets as oracles (e.g., LAN NTP
≤ 100 µs bound, GPS with PPS ≤ 2 µs, Starlink with the dish's server ≤ 1 ms, Starlink
without it reported honestly), checked in simulation and on HITL rigs.

---

## Q2. Own implementation, ntpd-rs or statime, or linuxptp and chrony?

**Recommendation:** build the mesh clock ourselves (time exchange over Foundation's own
transport, filtering, interval intersection, slewing, all as a pure state machine), read
GPS, PPS, PHC, and OS time daemons directly as sources, ship no PTP client in the first
release, and revisit statime's core or CSPTP for PTP once those crates stabilize.

Evidence:
- The exchange must ride Foundation's transport (relays, NAT traversal, mesh identity),
  not UDP 123. Its math is small: four timestamps, a minimum-delay filter, Marzullo
  intersection ([NTP clock select](https://www.eecis.udel.edu/~mills/ntp/html/select.html),
  [Wikipedia: Intersection algorithm](https://en.wikipedia.org/wiki/Intersection_algorithm)).
- ntpd-rs: `ntp-proto` says it "is not intended as a public interface at this time ...
  no stability guarantee" ([lib.rs](https://raw.githubusercontent.com/pendulum-project/ntpd-rs/main/ntp-proto/src/lib.rs));
  2.0.0-alpha (2026-07-15) "started on a large rework of the internals" ([CHANGELOG](https://raw.githubusercontent.com/pendulum-project/ntpd-rs/main/CHANGELOG.md));
  `clock-steering` is `cfg(unix)` only, no Windows ([lib.rs](https://raw.githubusercontent.com/pendulum-project/ntpd-rs/main/clock-steering/src/lib.rs)). [V2 from code]
- statime: core is platform-agnostic, the user supplies network and clock
  ([repo](https://github.com/pendulum-project/statime)), which passes T1. But the
  published crate is 0.4.0 from 2025-03-13, and development moved into the ntpd-rs repo
  (`statime-wire`, `statime-algo`, `statime-netptp` unpublished; `statime-base` 0.1.0
  published 2026-09-25; crates.io API). CSPTP is experimental in ntpd-rs with a 2026
  roadmap ([Trifecta Tech](https://trifectatech.org/projects/statime/), CHANGELOG). [V2]
- linuxptp and chrony are separate GPL daemons; linuxptp is Linux only, chrony has no
  Windows build. Driving them means extra processes, config files we must own, and a
  Linux-only feature set, against "one binary on every OS".

Rejected:
- Embedding ntpd-rs: unstable internal API mid-rewrite, Unix-only steering, wrong
  transport.
- Driving linuxptp or chrony as child processes: not one binary, Linux only (linuxptp),
  no Windows (chrony), configuration drift outside the spec.
- Writing a full PTP client now: best-master selection, profiles (default, 802.1AS),
  per-OS hardware timestamping, ports 319/320 privileges. Large, and only labs with
  PTP-aware networks benefit at first. Where a lab already runs ptp4l, we read its result
  (Q4) instead.

T1 fit: the estimator takes injected inputs (a raw clock reading, exchange timestamps,
source readings) and produces offset and bound. The simulator drives it with a drifting
clock and an asymmetric simulated network; nothing in it touches I/O.

Risks: a PTP-only lab (no NTP, no GPS) gets software-timestamp accuracy until a PTP
client ships, unless the OS already runs ptp4l/phc2sys.

**Decision for the user:** build the mesh clock in-house and defer a PTP client
(statime core or CSPTP) to a later phase, reading any existing OS PTP stack meanwhile.

---

## Q3. How should the mesh clock compute an honest error bound?

**Recommendation:** run the mesh clock on the raw monotonic oscillator, estimate its UTC
offset and frequency from interval-bounded exchanges, intersect sources with Marzullo,
grow the bound with elapsed time, and apply corrections by slewing only.

Design:
1. **Base clock:** `CLOCK_MONOTONIC_RAW` (Linux), `mach_continuous_time` (macOS),
   `QueryPerformanceCounter` (Windows). Mesh time = offset + (1 + freq) x raw. This is
   immune to other software stepping the OS clock; LabJack warns NTP can step host time
   by up to 60.5 s ([LabJack stream timing](https://support.labjack.com/docs/3-2-1-stream-timing-t-series-datasheet)).
   LSL uses the same idea: stamp with a local steady clock, then map with measured
   offsets ([LSL time sync](https://labstreaminglayer.readthedocs.io/info/time_synchronization.html)). [V2]
2. **One exchange:** four timestamps give offset θ and round trip δ. The true offset lies
   in θ ± δ/2 for any path asymmetry, so the honest bound is
   `ε = δ/2 + ε_source + ε_stamp`. This holds through relays and on Starlink; asymmetry
   only shifts where in the interval the truth sits. [V2: NTP select docs; ClockBound
   uses root delay / 2]
3. **Filter:** keep the last 8 exchanges per source and use the one with the smallest δ
   (NTP clock filter, [select.html](https://www.eecis.udel.edu/~mills/ntp/html/select.html)).
4. **Combine sources:** Marzullo intersection; sources whose interval misses the
   intersection are dropped as falsetickers; the result's midpoint and half-width are
   the offset and bound. [V2]
5. **Between exchanges:** ε grows by `max_drift x elapsed`. Start at a conservative
   200 ppm (TrueTime's assumption, giving its 1 to 7 ms sawtooth,
   [Spanner slides](https://www.cs.princeton.edu/courses/archive/fall16/cos418/docs/P6-Spanner.pdf))
   and tighten to the measured frequency stability plus margin. [V2]
6. **Slew, never step back:** S6 needs strictly increasing timestamps, so corrections
   apply at a bounded rate; the not-yet-applied part is added to ε, exactly as ClockBound
   adds `|local offset|` ([ClockBound](https://docs.amazonaws.cn/en_us/AWSEC2/latest/UserGuide/compare-timestamps-with-clockbound.html)).
   Step forward only at startup. [V2]
7. **Publish:** `<node>.clock.offset` and `<node>.clock.error` (half-width), so readers
   form [t - ε, t + ε] like TrueTime and ClockBound.

Worked example: a site on Starlink with δ = 87 ms gets ε ≈ 44 ms from a cloud source.
The same site using the dish's local server gets ε ≈ 1 ms or less. The bound itself tells
automation (Q4) which to prefer.

Rejected:
- Trusting the OS clock plus `adjtimex` maxerror alone: it moves when another daemon
  steps it, and Windows and macOS do not report a usable bound.
- Averaging offsets across sources: one falseticker (a bad server, a smeared leap second)
  pulls everything; intersection rejects it.
- Correcting by stepping: violates S6 ordering.

Risks:
- Suspend: `CLOCK_MONOTONIC_RAW` stops during suspend; on resume the node must reset its
  bound and re-sync before stamping. [U]
- Leap-second smearing: Google and AWS public servers smear; mixing smeared and unsmeared
  sources diverges by up to 0.5 s near a leap. Needs a policy (reject smeared, or smear
  everywhere). [U, not re-verified in this pass]

**Decision for the user:** base the mesh clock on the raw oscillator with slew-only
corrections and an interval bound computed as above.

---

## Q4. How does a node find its best sources with no configuration?

**Recommendation:** every node inventories candidate sources at startup and every few
minutes, measures each one's interval, and follows the tightest truechimer, so selection
is driven by measured bounds instead of a ranking table; the installer grants narrow
capabilities so better sources work without root.

Inventory:
- **Privileges:** Linux `capget` for `CAP_SYS_TIME`, `CAP_NET_BIND_SERVICE`,
  `CAP_NET_RAW`; Windows token check for `SeSystemtimePrivilege`
  ([SetSystemTimeAdjustmentPrecise](https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-setsystemtimeadjustmentprecise));
  macOS effective uid.
- **NIC hardware timestamps:** Linux `ETHTOOL_GET_TS_INFO` via `SIOCETHTOOL`, which is
  unprivileged and returns the PHC index ([kernel commit](https://git.linaro.org/plugins/gitiles/kernel/linux-linaro-stable.git/+/c8f3a8c31069137fe0100e6920558f1a7487ef3c%5E%21/net/core/ethtool.c),
  [netdev](https://lists.openwall.net/netdev/2012/04/04/67)) [V2]. Windows
  `GetInterfaceActiveTimestampCapabilities`, build 20348 and later [V2]. macOS: none.
- **PHC access:** systemd udev rules put `/dev/ptp*` in group `clock`, so reading a PHC
  needs no root ([RHEL-74548](https://issues.redhat.com/browse/RHEL-74548),
  [PipeWire MR](https://gitlab.freedesktop.org/pipewire/pipewire/-/merge_requests/1847)) [V2].
  If ptp4l already disciplines a PHC, read it against our raw clock as a source.
- **PTP on the wire:** passively listen for Announce messages on UDP 319/320. Linux needs
  `CAP_NET_BIND_SERVICE` for ports below 1024; macOS lifted the restriction in Mojave
  ([Apple forum](https://developer.apple.com/forums/thread/674179)); Windows has none.
  Seeing a grandmaster is reported as a status channel until a PTP client exists.
- **GPS:** gpsd on `127.0.0.1:2947`; serial ports probed for NMEA RMC sentences at common
  baud rates; PPS through Linux `/dev/pps*` (RFC 2783 PPS API; Linux, FreeBSD only,
  [NTP PPS docs](https://docs.ntpsec.org/latest/pps.html)). Windows and macOS get NMEA
  only (ms class).
- **Known local servers:** the Starlink dish at `192.168.100.1:123` (Q1); NTP servers
  from DHCP option 42; the default gateway. [U for DHCP option discovery per OS]
- **OS clock as one more source:** Linux `adjtimex` with `modes = 0` is unprivileged and
  returns `maxerror`, `esterror`, and `STA_UNSYNC` ([man7 adjtimex](https://man7.org/linux/man-pages/man2/adjtimex.2.html),
  [mankier](https://www.mankier.com/2/adjtimex)) [V2]; macOS `ntp_adjtime` read.
- **Install-time grants:** the Linux unit uses `AmbientCapabilities=CAP_SYS_TIME
  CAP_NET_BIND_SERVICE CAP_NET_RAW`, the systemd-timesyncd pattern
  ([Rocky hardening](https://docs.rockylinux.org/guides/security/systemd_hardening/),
  [timesyncd unit](https://git.alternativebit.fr/picnoir/Systemd/src/commit/65be7042a876ffe186a42ced04cde60ed81d3136/units/systemd-timesyncd.service.in)) [V2],
  plus membership in `clock`. OS steering stays off unless the time policy turns it on.

Selection: each candidate becomes a source with a measured interval; Marzullo picks
truechimers; the node follows the smallest bound. The `[[time]]` policy (C6) only
overrides. The node publishes which source it follows and why on its status channels.

Rejected:
- A fixed priority table (GPS > PTP > NTP): a GPS with no PPS on Windows is worse than a
  LAN NTP peer; measured bounds rank correctly without special cases.
- Requiring root: the capabilities and `clock` group give everything needed.

Risks: serial probing can disturb other serial devices (a PLC on RS-485); probing must
be read-only and skip ports a connector owns. Windows hardware timestamp drivers are
unreliable.

**Decision for the user:** bound-driven automatic selection with install-time
capability grants and no ranking table.

---

## Q5. Device clocks (DAQmx, LabJack)

**Recommendation:** connectors model each device sample clock as an oscillator, fit its
offset and rate against mesh time from the lower envelope of read-completion times, use
device-side timestamps where the hardware offers them, never step backward, and write the
remaining error to the index's error channel.

What the devices offer:
- **DAQmx:** without special hardware, t0 comes from one read of the PC clock adjusted by
  samples acquired, then t0 + n x dt; the ~50 ppm timebase accumulates ~5 s/day
  ([NI forum](https://forums.ni.com/t5/LabVIEW/Can-I-rely-on-DAQmx-timing/m-p/4286414)) [V1,
  forum; consistent with the ledger]. `FirstSampTimestamp.Enable/Val` gives the first
  sample clock pulse time for hardware-timed tasks, on by default for AI and DI
  ([NI property](https://ni.com/docs/en-US/bundle/ni-daqmx-properties/page/daqmxprop/attr3139.html)).
  TSN chassis (cDAQ-9185/9189) sync by 802.1AS to < 1 µs; host time and I/O device time
  are separate timescales, past timestamps translate "within a millisecond" unless the
  host is in the 802.1AS domain ([NI TSN guide](https://www.ni.com/en/shop/data-acquisition/how-to-achieve-high-accuracy-measurements-with-ni-daqmx-based-ts.html)) [V2].
- **LabJack T7:** 20 ppm clock; `CORE_TIMER` at 40 MHz (32-bit, wraps ~107 s) can be added
  to the scan list to stamp each scan; LabJack recommends re-sampling host time
  periodically; external scan clock possible
  ([stream timing](https://support.labjack.com/docs/3-2-1-stream-timing-t-series-datasheet),
  [LabJack forum](https://files.labjack.com/utilities/archive/forum_html_backup/forums/t7/time-stamping-streamed-data.html)) [V2].

Method:
1. Each read gives a pair (index of last sample, mesh time when the read returned). The
   sample happened before the return, so every pair is an upper bound. Fit the line
   under the points (minimum latency), the device equivalent of NTP's minimum-delay
   filter. The fit's rate absorbs 50 to 120 ppm drift.
2. Where the device has its own counter (LabJack `CORE_TIMER`, DAQmx first-sample
   timestamp), map device time to mesh time by bracketing a counter read with two mesh
   timestamps: bound = half the bracket, the same math as Q3. [U: whether T7
   command-response reads are allowed while streaming]
3. Apply rate and offset changes smoothly; timestamps never go backward (S6).
4. Write error = fit residual bound + mesh clock ε to the index's error channel (S6).
5. For sub-µs needs, discipline the hardware: TSN chassis, PXI reference clocks, or a
   GPS-disciplined external scan clock on the LabJack.

Synnax today: both NI and LabJack hardware-timed tasks use `HardwareTimedSampleClock`
(`driver/common/sample_clock.h:144-216`; `driver/ni/read_task.h:245`,
`driver/labjack/read_task.h:626-650`): first timestamp = system time at the first wait,
then a fixed n x dt increment pulled toward read-return time by a PID controller. The
class has P, I, and D terms with an integral cap and a back-correction cap, but the
production clock runs P only: Kp = 0.01, Ki = Kd = 0, back-correction capped at half a
stream period (r18). It tracks the PC's realtime clock (affected by steps), is biased
late by transfer latency because read-return time is treated as the sample time, and
reports no error.

Rejected:
- Keeping Synnax's PID controller (P only as deployed): latency bias, no bound,
  coupled to OS clock steps.
- Trusting DAQmx t0 + n x dt: drifts ~5 s/day.
- Re-reading the host clock per read without a model: injects USB and scheduler jitter
  (ms) into every timestamp.

Risks: the envelope fit needs a warm-up (LSL notes 30 to 120 s before dejittering is
under 1 ms, [LSL](https://labstreaminglayer.readthedocs.io/info/time_synchronization.html));
the bound must be wide during warm-up.

**Decision for the user:** connectors use the oscillator model with lower-envelope
fitting and publish the device error, replacing Synnax's PID controller.
