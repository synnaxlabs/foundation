- **R6 TIME LOCKED** Own sans-I/O estimator over our transport. The bound is half the
  round trip. Keep the fastest exchange per source, combine sources, widen the bound
  with drift, slew only. Sources are read directly: mesh peers, GPS, PPS with NMEA, the
  NIC hardware clock (kept by ptp4l), and the OS daemon. No PTP client in v1. Device
  clock fitting (DAQmx, LabJack) is a connector-library component that writes residual
  error to the index's error channel. Amended by MESH SLEW: mesh time also steps
  forward when it is more than 500 us behind every offset an estimate allows.
