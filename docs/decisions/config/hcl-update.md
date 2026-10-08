- **HCL UPDATE (2026-10-05)** `config_hcl::update` changes a file so that it reads as
  a new Document. Each attribute and block that keeps its value and its place keeps its
  bytes, comments, and blank lines. A changed value and a changed block on one line
  are written again, without the comments in them. A removed item is cut with the
  comment lines directly above it, up to a blank line. A new attribute goes after the
  kept attribute before it in key order, and a new block after the kept block before
  it. The k-th block of a keyword and labels pairs with the k-th new one, and the
  longest run of pairs in the same order stays, so a moved block is cut and written
  again. New text takes the file's line end. Lost: an edit list by span, which puts
  the diff on each caller; returning edits, which each caller must apply; a lossless
  syntax tree with comments as trivia, which needs a second tree type in the reader;
  writing the whole file with comments attached to items, which loses the layout; and
  moving the bytes of a moved block, which a caller that changes the Document it read
  never needs. Decided by the `config` builder; approved by the coordinator (#249).
