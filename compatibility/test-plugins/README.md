# Synthetic legacy test plugins

Hand-written Keypirinha-API plugins that exercise one scheduling or lifecycle
behaviour each. They are deliberately adversarial and run in CI (spec 27.2, 27.3):

- a plugin that never checks `should_terminate()`
- a plugin whose `on_suggest` sleeps far past the modern hard deadline
- a plugin that publishes very large `set_suggestions()` lists
- a plugin that crashes the interpreter mid-callback
- a plugin that relies on activation/deactivation coalescing
