Like Cargo, but for C(++)
=========================

Important: this is just a proof of concept! PRs welcome.

Auto-builds a project if convention is followed:

* All .c(pp) files (except "root" file) have a header file (.h or .hpp).
* All header files have a .c(pp) file of same name
* There exists a single "root" file which includes headers in its dependencies

Like in case of Cargo, the build configuration is in Gocar.toml See the example

How to try it out
-----------------

1. Make sure you have [Rust](https://rust-lang.org) installed. This tool uses `impl Trait` feature, so you need a recent version of the Rust compiler. Use `rustup update` to update if you have old version already.
2. `git clone https://github.com/Kixunil/gocar`
3. `cd gocar`
4. `cargo build --release`
5. `cd example_c_project`
6. `../target/release/gocar build`

The last command builds the `example_c_project`, so you can run it with `./example`

Planned features
----------------

* Cleaner code, no hard-coding
* Support for headers without c(pp) files
* Platform-dependent compilation (e.g. have a header named "foo.h" and implementations "foo-linux.c", "foo-macos.c")
* Pre-build and post-build scripts
* Dependency management
* Features (like in Rust)
* Options (like features, but not additive)
