- **SPECIFICITY (#3)** Pattern specificity orders by more literal segments, then fewer
  `**`, then more `*`: `a.b` > `a.*` > `a.*.**` > `a.**` > `**`. A run of wildcards
  counts as its `*`s and one `**` (`a.**.*.**` is `a.*.**`). Two different patterns may
  tie (`a.*` and `*.a`); a tie between the most specific setting policies on one name is
  the S12 plan error. For node settings, the plan error is a tie between the most
  specific policies that set one budget for one node (X25). A tie below them decides
  nothing, because only the most specific value is used. Access has no ties (X25). The
  tie rule is the reading of S12 by `laptop.architect-2` (2026-10-07T15:16:46Z:
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6040858277), and
  `laptop.director` agrees that it changes no rule (2026-10-07T15:27:12Z:
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6041077733).
