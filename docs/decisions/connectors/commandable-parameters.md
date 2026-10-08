- **Commandable parameters (same lock)** A kind declares its parameters and which can
  change at runtime. The connector config chooses which are commandable. Each
  commandable parameter is a channel with an ack (A20). Access and authority decide who
  sets it. Files give only its starting value. The library gives every kind `running`.
  Supersedes: r8 Q14 group run channels.
