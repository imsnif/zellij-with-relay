# zellij-ansi-clip

A standalone ANSI clipping / padding state machine used by Zellij's relay-side
read-only viewer path. The crate parses the subset of CSI sequences that
`zellij_server::output::Output::serialize_with_size` emits, maintains an
in-memory grid at the full session viewport, and emits a new ANSI byte sequence
clipped or padded to an arbitrary viewer size.

## Building the wasm blob

The browser consumes a wasm build. Produce it via the xtask helper:

```bash
rustup target add wasm32-unknown-unknown
cargo x build --wasm-clip -r
```

The build copies the optimised output to
`zellij-web-client-assets/assets/clip.wasm`. `wasm-opt` is used when available;
otherwise a plain copy is made.

## Regenerating the fidelity fixture

The integration test `t17_fidelity_anchor_40x120` validates the clipper against
`tests/fixtures/full_session_40x120.ansi`, a captured `serialize_with_size`
output. Regenerate via:

```bash
cargo test -p zellij-server regenerate_clip_fixture -- --ignored --nocapture
```

If the fixture file is absent the test falls back to a synthetic capture built
inline, so the test suite runs green on a fresh clone.
