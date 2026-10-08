- **S9 (changes log)** A built-in changes channel carries the small change records; seq
  is the Raft log index; any copy can serve it; readers resume from any source. There
  is one per region (X29).
