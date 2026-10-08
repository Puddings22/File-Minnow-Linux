# Notes for coding agents

Read [README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md) first; the principles
there apply. In addition:

- Run `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`
  and `cargo test --locked --all-targets` after code changes. Keep
  `--no-default-features` building, in a separate target directory so it does not
  replace the GUI test binary.
- The person running you is probably using the same desktop. Do not take screenshots,
  move focus, open windows or send input on their display. Use `scripts/test_ui.py` and
  `scripts/test_focus.py`, which run on a private Xvfb display and D-Bus session.
- On X11, window activation goes through `desktop::x11_activate` (a pager-style
  `_NET_ACTIVE_WINDOW` request). A plain `present()` is refused by focus-stealing
  prevention when the request comes from the global shortcut, the tray or a second launch.
- Measure performance at realistic scale (1M+ entries, long paths) with
  `examples/bench_index.rs`, `examples/bench_update.rs` and `scripts/measure_gui.py`,
  not only on small fixtures. Do not claim numbers you have not measured.
- Do not claim full compatibility with Everything; document differences.
