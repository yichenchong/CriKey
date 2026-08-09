# WebAssembly conformance fixture

This is a standalone, third-party-shaped plugin. It depends on the shared ABI
with the interpreter feature disabled, and therefore can be built for
`wasm32-unknown-unknown` without compiling `wasmi` into the guest:

```text
RUSTFLAGS='-C link-arg=--export=memory' \
  cargo build --manifest-path compatibility/wasm-conformance/Cargo.toml \
  --release --target wasm32-unknown-unknown
```

The resulting `target/wasm32-unknown-unknown/release/crikey_wasm_conformance.wasm`
exports `crikey_abi_version`, `crikey_alloc`, `crikey_suggest`,
`crikey_catalog`, `crikey_execute` and `memory`. `crikey_suggest` returns one
item whose label includes the query. It imports no host capability, so it also
proves that the no-permissions sandbox can load and answer.

An installed package's `crikey.toml` points `entrypoint` at this module and
sets `runtime = "wasm"`; `crikey-app` then launches `crikey-wasm-host` under
the ordinary native supervisor. The guest never executes in the UI process.
