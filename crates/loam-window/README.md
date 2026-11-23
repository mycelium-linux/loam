# loam-window

A native window for a Spore web surface.

`loam-window` owns the window and an OS webview. It receives only a URL, title,
and size; it never receives a component handle, filesystem path, credential, or
capability. The Spore serves HTML over the URL the host gives it.

```sh
cargo build --manifest-path crates/loam-window/Cargo.toml
crates/loam-window/target/debug/loam-window \
  --url http://127.0.0.1:8799/ \
  --title "probe.dev"
```

Options:

```text
--url URL       page to load
--title TITLE   window title
--size WxH      logical window size, default 1000x700
--devtools      enable webview devtools
```

The crate is intentionally outside the root workspace: `wry` needs platform
webview libraries on Linux, and the core Loam workspace should still build on a
headless host without WebKitGTK installed.
