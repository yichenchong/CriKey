# Built-in components

This directory is reserved for first-party built-in packages. It currently
contains no plugin package files. The shipped application catalog is composed
by `crikey-cli` and `crikey-app` rather than loaded from this directory, and it
runs in the main process under first-party code rules.

Third-party plugins never run in the main process. Planned built-ins include:
application catalog extensions, filesystem navigation, calculator, URL/web
search, clipboard history, and CriKey control commands.
