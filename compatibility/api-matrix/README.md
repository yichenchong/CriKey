# Legacy API compatibility matrix

Version-controlled and tested classification of every documented Keypirinha
API surface CriKey's Legacy Compatibility Layer implements (spec 14.10).

Each entry is classified as one of: `full`, `behavioural-difference`,
`windows-only`, `partial`, `unsupported`, `planned`.

The matrix is data, not prose: `matrix.toml` is consumed by
`crikey dev test-legacy-compat` and by the compatibility test suite.
