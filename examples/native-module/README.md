# native-module

A consumer binary over `mintworks::Host` that adds a native Rune module, `greet::`, written in
Rust next to the framework's own. Its Rune app in `app/` calls `greet::hello` from a route, and
`tests/native.rs` runs that app's suite through `Host::run_tests`.

It is a workspace member, so `cargo test --all` covers it; on its own:

```sh
cargo test -p native-module
```

To serve the app, `cargo run -p native-module -- examples/native-module/app`.
