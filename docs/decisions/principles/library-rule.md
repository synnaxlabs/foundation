- **LIBRARY RULE** Choose the architecture first, then a library or our own build, for
  every major dependency. A library that does its own I/O or reads the clock fails T1.
  Protocol cores (consensus, wire session, control gate, spec sync, clock offset) are
  sans-I/O state machines with thin drivers. Connectors are not.
