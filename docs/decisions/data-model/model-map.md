- **MODEL MAP (current)** Data channel -> index, -> quality (optional), -> data type,
  -> unit. Index -> error channel (optional), -> control channel (optional). Type ->
  other types; types never point at channels. Policies -> names through selectors;
  channels never point at policies. Readers and connectors -> channels by name or
  selector. Calculation -> inputs, -> its own output index. Connector -> channels it
  reads and writes; channels never point at connectors. Region -> name prefix. Node,
  connector, subject, and channel names share the one name tree. A channel's edges stay
  in its region (REGION CHECK; `laptop.architect`, 2026-10-08T08:43:31Z,
  https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056144931).
