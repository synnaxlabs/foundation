- **SIM SERIAL (2026-10-05)** `Sim::line` joins two node ports with a serial line.
  Bytes go at the sender's `Settings::rate`, and an end with other settings gets
  random bytes. Each line draws its faults (loss, a flipped bit) and its random bytes
  from its own stream as each byte is sent, so a change of the line acts only on the
  bytes sent after it. A flip with parity on is lost. Each port holds 4 KiB to send
  and 4 KiB to read, as a Linux TTY does. An open ends at once. `Node::fail_serial`
  makes a port fail as a pulled USB adapter does: each read and write gives `EIO`
  until the port drops. Built by `simulation` in #431 and #690.
