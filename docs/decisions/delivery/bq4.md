- **BQ4** Encode once at entry: the home encodes local writes, and the writer's `hub`
  encodes remote ones. Decode once at the reader. Each series decodes by itself. The
  home checks data series by their headers only and decodes index series.
