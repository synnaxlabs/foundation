- **NAME LENGTH (2026-10-04)** A name or pattern holds at most 255 bytes. The person
  chose "255 bytes": it fits a one-byte length prefix, and raising it later stays
  backward compatible. The `!` of an exclusion is syntax: the exclusion's pattern is
  the text after it. Decided by the advisor on 2026-10-06, #762.
