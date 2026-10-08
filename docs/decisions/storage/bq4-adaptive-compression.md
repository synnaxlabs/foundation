- **BQ4 + ADAPTIVE COMPRESSION (r10)** Per 1024-value vector, one stats pass gives exact
  sizes, and the encoder picks the smallest. Raw is always a candidate. The minimum
  saving is a fixed 1/8. No sampling and no hysteresis, except ALP's top-5 exponent
  pairs, refreshed about every 100 vectors. Codec set: integers raw, FFOR, delta
  (natural order), RLE; timestamps add stride; floats raw, ALP, fdelta, RLE; `max` mode
  adds pco. No zstd and no ALP_rd. Policy: `compression { select, mode = auto, raw, or
  max }`, default `auto`. The validator is the top fuzz target.
