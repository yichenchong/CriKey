# Built-in components

This directory is reserved for first-party built-in packages. It currently
contains no plugin package files. The shipped built-ins are composed by
`crikey-cli` and `crikey-app` rather than loaded from this directory, and they
run in the main process under first-party code rules: the application catalog,
the file search (`builtin.crikey.files`), and the calculator
(`builtin.crikey.calculator`, `crates/crikey-cli/src/calculator.rs`).

Third-party plugins never run in the main process. Planned built-ins include:
application catalog extensions, filesystem navigation, URL/web search,
clipboard history, and CriKey control commands.
