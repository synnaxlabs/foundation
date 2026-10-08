- **BQ11a** Joining is an operation. An admin creates a join ticket (single-use or
  reusable, with an expiry, scoped to a region and name prefix). The node joins with it,
  voters record membership, and the join is logged on the changes channel. Files name
  nodes only where they matter (voters, placement). Ephemeral nodes are removed after a
  set time offline. Tickets are secrets.
