# Artifact Dependency

While [issues](https://github.com/rust-lang/cargo/issues/9096) are being resolved,
there are no artifact dependencies (on cdylibs, binaries, staticlibs, etc) supported
through cargo. This crate provides some lightweight utilities to build and use pseudo
artifact dependencies.