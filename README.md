Like Cargo, but for C(++)
=========================

Important: this is just a proof of concept! PRs welcome.

Auto-builds a project if convention is followed:

* All .c(pp) files (except "root" file) have a header file (.h or .hpp).
* All header files have a .c(pp) file of same name
* There exists a single "root" file which includes headers in its dependencies

Planned:

* Cleaner code, no hard-coding
* Support for headers without c(pp) files
* Configuration in Gocar.toml
* Platform-dependent compilation (e.g. have a header named "foo.h" and implementations "foo-linux.c", "foo-macos.c")
* Pre-build and post-build scripts
* Dependency management
* Features (like in Rust)
* Options (like features, but not additive)
