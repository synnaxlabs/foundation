- **T1 seam** Foundation's own `Transport` trait sits in front of every carrier.
  Amended by SIM NETWORK: the trait is private to `transport`, and `sim` replaces the
  network below the carriers (`env::net`), not the transport.
