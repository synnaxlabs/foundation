- **MESH SLEW (2026-10-05)** After the first estimate, mesh time moves toward each new
  estimate at no more than 500 ppm (ntpd's maximum slew), in `estimate::Slew`. The part
  not yet applied goes into the error, so a slew of 1 s takes 2000 s and its error says
  so. When the offset served at `now` is more than 500 us (1 s of slew) below the
  earliest offset the new estimate allows at `now`, mesh time steps forward to that
  earliest offset. In every other case it slews. Mesh time never steps back. A clock in
  holdover keeps its slew. Cost: mesh time that is ahead still slews. After a stale
  first estimate that is ahead, or for an estimate with an unknown error (a Windows OS
  clock alone under OS CLOCK BOUND, whose earliest offset is 36500 days back), a
  correction of 1 h takes 83 days and one of 1 day about 5.5 years, with a true error
  the whole time. A majority of falsetickers more than 500 us ahead steps mesh time into
  the future, and it does not come back. That is outside the fault model. Lost: a
  frequency loop (a PLL, as in ntpd), because R6 bounds drift with an error that grows
  and a PLL can overshoot; the slew private in `clock`, because it is decision logic in
  layer 2; a step only when the estimate's whole interval is ahead of mesh time's,
  because when both bounds hold the intervals overlap and it never fires. Amends R6 TIME
  LOCKED ("slew only") and r6 Q3 item 6 ("Step forward only at startup"). The person
  decided on 2026-10-05 ("Ok 225 mesh slew approved"), with the forward step. The person
  changed the forward step on 2026-10-05 ("I think (b)"), because the first rule never
  fires when both bounds hold.
