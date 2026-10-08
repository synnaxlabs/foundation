# Layer rules


**X44. C1 rule 1 ("layer 3 uses only `hub`") vs what connectors need.**
Conflict: connectors also need `types`, `block`, `spec`, `document`, the estimator,
and `env` seams.
Resolution: layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
`ctx` hands out the `env` seams. Basis: C1, BQ3, BQ5.

**X45. C1 rule 2 ("only layer 2 does I/O") vs connectors, front ends, `node`, and
secret adapters.**
Conflict: connectors open sockets and call vendor libraries; front ends and `node`
read files; secret adapters call Vault and cloud services.
Resolution: layer 1 does no I/O. Layer 2 does mesh I/O (peers, the disk buffer, the
clock). Layer 3 does outside-system I/O only through injected dialers, links, and
dedicated vendor threads. Layer 4 does process, file, and OS I/O. All of it enters
through injected seams, so simulation can replace it. Basis: T1, SRP PASS.

**X46. Where access is enforced.**
Conflict: BQ5's lock text says "hub and home enforce the rules (... access)". r8 Q12
and r12 enforce access only at the owner. BQ12 found that "authenticate at `hub`,
authorize at the owner" lets a forwarding node impersonate a subject.
Resolution: enforcement only at owners (`home` for data; region voters for apply,
secret, admin), with no check in `hub` or `ctx`. The owner learns the true subject
from the signatures it verifies (BQ12). Basis: root "no defense in depth", r12 table.
