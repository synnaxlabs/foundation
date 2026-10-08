- **READ COPIES** Placement `copies = [node]` keeps a never-promoted copy of an index,
  fed by `replica`, in any region. Remote readers read and hold at the copy, so a weak
  link carries each sample once.
