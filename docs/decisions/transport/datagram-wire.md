- **DATAGRAM WIRE (#55, 2026-10-05)** On QUIC, a datagram is one message in one QUIC
  DATAGRAM frame. `transport` adds no prefix: the frame carries the length, and the
  message itself starts with the STREAM DISPATCH header, which the caller writes. A node
  takes datagrams on every connection. It sends its `message_bytes_max`, at most 65535,
  as the QUIC `max_datagram_frame_size` parameter. noq-proto holds at most
  `message_bytes_max` bytes of datagrams until the node takes them, after each UDP
  packet, so `message_bytes_max` is at least 1472, the largest UDP payload a node takes
  (#610). A sender's largest datagram is the smaller of the path's limit and the peer's
  limit less the frame header (9 bytes). A peer that takes no datagrams, such as an SDK
  client, gets none: the limit is 0, and each send gives `Error::TooLarge`. A datagram
  over the receiver's `message_bytes_max` breaks the protocol: noq-proto closes the
  connection with PROTOCOL_VIOLATION, and the caller gets `Error::Broken`. Each
  connection queues at most 64 KiB of datagram bytes to send; when a new one does not
  fit, the oldest unsent ones drop. Small datagrams hold more pool than that, because
  each holds a block (#615). A node copies each datagram into a block from its pool when
  it arrives, and drops it when it gets no block (the pool or the system has no room).
  At most 64 wait untaken on one connection; a new one drops the oldest, and they free
  when the connection ends. #68 adds a count of each drop, with the counts of RECV
  WAITS. Proposed by `network` in #55; approved by the coordinator on #55, and the
  `datagram` doc on #565.
