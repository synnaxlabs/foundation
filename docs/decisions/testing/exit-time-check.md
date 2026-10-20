- **EXIT TIME CHECK (2026-10-18)** Exit test 1 of ONE NODE and the tests of ONE NODE of
  a wrong endpoint (`a_wrong_endpoint_shows_in_status_and_heals`) and of a Bad value
  (`a_bad_value_is_left_out_and_counted`) check the time of each sample that InfluxDB
  holds (in exit test 1 and the test of a wrong endpoint, to the last step of the value)
  against the time at which the server answered its read. The offset is the stored time
  less the served time. The window `W_k` of read `k` is the served time of read `k+1`
  less that of read `k-1`, of each read of the variable, Good or Bad; for read 0, from
  the start of the node. Each sample holds `|offset| <= W_k / 2 + BOUND` (the hold): the
  stamp is the midpoint of its read (STAMP), and the read lies inside its window. Each
  sample of a read after read 0 whose window is at most `2 * BOUND` holds
  `|offset| <= BOUND`, the 1 s target of the MVP (the target). A wider window shows a
  read too slow for the target, and no stamp rule meets it with one request. Read 0
  takes only the hold, since its window holds the start of the node. In each of the
  three tests, sample `k` stored has the value of read `k`, and read `k` is Good. The
  three tests run the `opcua` kind with `polled = true`, which writes a sample of each
  read (`laptop.monitor`, kinds d and g of #3507, quoted at 2026-10-20T06:49:37Z in
  https://github.com/synnaxlabs/foundation/pull/3507#issuecomment-6285161214). A
  failure gives the offset and window of each checked sample. After #2627, the hold
  takes the error that the node writes for the sample in place of `W_k / 2`. A clock
  stall at the start of the node gets its own issue when a failure shows it, with its
  plan to the architect of `clock`. Decided by `laptop.architect-2`
  (2026-10-18T16:15:17Z):
  https://github.com/synnaxlabs/foundation/issues/3198#issuecomment-6252160050. The test
  of a Bad value, by `laptop.architect-2` (2026-10-18T16:56:22Z):
  https://github.com/synnaxlabs/foundation/pull/3224#issuecomment-6252582865. The check
  by index in the test of a Bad value, by `laptop.architect-2` (2026-10-18T17:16:19Z):
  https://github.com/synnaxlabs/foundation/pull/3224#issuecomment-6252788106. Supersedes
  the pass on some Good read `k` of
  https://github.com/synnaxlabs/foundation/pull/3224#issuecomment-6252582865. Lost: a
  bracket, the stamp of read `k` between the served times of reads `k-1` and `k+1` plus
  or minus `BOUND`, which lets the offset grow with each stall and drops the 1 s target.
  Lost: a check of the test of a Bad value apart, with `BOUND` for each read, which
  keeps the failure of #3198 at read 0 and a second limit for one claim. Lost: in the
  test of a Bad value, a pass on any Good read of the value of the sample, at a time
  within the limit of that read, which takes the union of the limits of the reads before
  the changes and does not check the stamp of a read against its served time.
