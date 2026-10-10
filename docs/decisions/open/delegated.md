# Settled under a delegation

On 2026-10-05 the person gave every open decision to the advisor and the
coordinator: "Don't block any decisiosn on me. consult with the advisor and come toa
conclusion together". Each one is listed below.

- Quality: X10 (ack quality on the ack's index), X19 (death record scope), R16-1 and
  R16-3 to R16-9 (r16 Rust guides).
- Memory and performance: X8 (seq per index group), X30 (merge rule), X42 (interner),
  S4 disk format starting point and its ring sizing (#637), `Layout::new` refuses a
  `body_max` under one block less the record header (#627), r12 I4 (`buffer`
  driven, not self-running), `ring` holds its own unsafe slot code
  (`docs/decisions/crate-map.md`).
- Failover: X18 (gate start from log records, R13-5 "held, not connected" grace), X43
  (copy mode), R13-10 (three voters for failover; `plan` warns with fewer), R13-6 (send
  after sync vs on receipt), #719 ("A PreVote answer, grant or refusal, shows the
  voter's state when it sent the answer."), #352 item 1 (a reply from a node that is not
  a peer).
- Names: X11 (`estimate`, `stamp`), X12, X29 (`@changes`), X47 to X50, X52, the
  tree key `<label>.@<kind>` of a policy (#729), `frame::split`, which cuts a frame
  body at its ends and gives each part (#632), HCL REFERENCES first segment (#536),
  generated names as strings (#701), and POLICY NAMES (#474).
- Delivery and wire internals: RECV WAITS (#581), the STREAM WIRE room order (#611),
  the STREAM WIRE hello (#55), a reader session key type per mode and the drop of a
  late reader call (#725).
- Architecture: X17 and `docs/decisions/crate-map.md` (`env`, `document`, `estimate`,
  `secret` crates), X21, X44, X45; R12-3 error classes without groups; R12-7 vendor code
  only in dedicated, never-detached threads; R12-13 no always-on scan loop; R12-14 one
  cycle engine per connector; SHARD PIN (#718), the advisor's choice A narrowed to a
  bool; an error below `document` that a producer shows as a diagnostic (DIAGNOSTICS)
  has `Display` and `fix()` and no `Code`, and the grammar of a value has one home, in
  `types` (advisor, #328).
