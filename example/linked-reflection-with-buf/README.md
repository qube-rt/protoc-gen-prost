# Reflection across crates with `prost_reflect=linked`

This example generates two crates with [buf] and `protoc-gen-prost`'s
`prost_reflect=linked` option, the messages of one holding those of the other,
and reads both through prost-reflect: text format, and the extensions set in
their options.

[buf]: https://buf.build

## Problem

With `prost_reflect`, each module embeds the descriptors of its files and of
all the files they import, so every module importing another crate's protos
carries a copy of that crate's descriptors.

With `prost_reflect=linked`, each module embeds its own files only, and gets a
`register_descriptors()`. It adds them to prost-reflect's global pool after
calling the `register_descriptors()` of the crates the `extern_path`s point to.
The `ReflectMessage` implementations call it before reading the pool. Every
crate an `extern_path` points to must be generated with `prost_reflect=linked`
too.

## Layout

| Path | Role |
| --- | --- |
| `proto/example/base/base.proto` | Messages of the base crate, a nested one, a well-known type and a field option |
| `proto/example/options/options.proto` | An extension only, so no `extern_path` maps it: the base crate embeds it |
| `proto/example/derived/derived.proto` | Messages of the derived crate, holding the base crate's |
| `base/buf.gen.yaml` | Generates `example/base` into the base crate |
| `derived/buf.gen.yaml` | Generates `example/derived`, with `extern_path=.example.base` pointing at the base crate |
| `derived/src/lib.rs` | Tests: text format across the crates, option extensions, each file registered once |

## Generate and test

Build the workspace plugins first (from the repository root):

```shell
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
```

Then generate each crate and test the example:

```shell
cd example/linked-reflection-with-buf
(cd base && buf generate)
(cd derived && buf generate)
cargo test --locked
```

CI runs the same steps in the `linked-reflection-with-buf` job (see
[`.github/workflows/rust.yml`](../../.github/workflows/rust.yml)).

## Related

- [`prost_reflect=linked` in the `protoc-gen-prost` README](../../protoc-gen-prost/README.md)
- [`example/wkt-reflection-with-buf`](../wkt-reflection-with-buf): reflection for a gRPC service
