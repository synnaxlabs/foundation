- **ARM RUNNER (2026-10-04)** CI runs every test on aarch64 too, because a wake protocol
  can pass on x86 and fail on ARM (r11 4.1). The person chose "AWS runner always on" and
  said "I have tons of AWS credits". Three runners (`foundation-arm-a`, `-b`, `-c`)
  share one AWS m7g.2xlarge (8 vCPU, 32 GiB) in us-east-1 with no inbound ports, tagged
  `project=foundation-ci`, outside BENCH SPEND. One runner queued 9 runs while its host
  used about 30% CPU, so the person asked: "can we have multiple runners on a single
  machine?" ARM skips docs-only changes. The coordinator owns it. On 2026-10-05 the
  host ran at 80 to 86% CPU with 14 runs queued, so a second host, an m7g.4xlarge (16
  vCPU, 300 GB) with six runners (`foundation-arm-d` to `-i`), joined it. The person
  chose "m7g.4xlarge, 6 runners". On 2026-10-06, with 55 runs queued, a third host
  joined with three runners (`foundation-arm-j` to `-l`): one spot machine from an EC2
  Fleet over six Graviton types and six zones (launch template `foundation-arm-spot`),
  because AWS took back a single-type spot machine after 30 minutes. Limits: a spot
  price cap of 0.20 USD/h, and a hard stop on 2026-10-08 at 03:00 UTC. With it, the
  hosts and the factory host cost at most 99.73 USD a day (#15). The person said:
  "Once you are sure of costs provision and set strict limits on whatever you need
  please". With 51 runs still queued, a fourth host joined with twelve runners
  (`foundation-arm-m` to `-x`): one 32-vCPU spot machine from a fleet over eight
  Graviton types and five zones (launch template `foundation-arm-spot-32`). Limits: a
  spot price cap of 0.60 USD/h, a hard stop with the factory host on 2026-10-07 at
  07:03 UTC, and a cap of 18 USD (#15). Until that stop, the daily cap is 115 USD; then
  it is 100 USD again. The person said: "Yes thats fine".
