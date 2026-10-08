- **BQ11b rule** A crate's value is the source of truth. A channel is a published copy
  for people, agents, and outside tools. Core decisions (failover, fencing) never read
  channels back. Upward flow goes only through values that the upper crate pulls.
