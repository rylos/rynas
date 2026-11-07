# Contributing to rynas

Thank you for helping. Bug reports, ideas and pull requests are welcome on
[GitHub](https://github.com/rylos/rynas).

## Before you start

- Open an issue first for anything larger than a small fix, so we can agree on the approach.
- Code, comments, docs and the default UI language are in English; UI texts also need their
  Italian translation (`crates/rynas-ui/src/i18n.rs` and `crates/rynas-ui/assets/i18n/it.txt`).
- Every feature must work on plain Debian with standard tools; hardware-specific code goes in
  its own module.
- Run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`
  before opening a pull request.

## License of contributions

By submitting a contribution you confirm that you wrote it or have the right to submit it.
You keep its copyright, and you grant Marco Ziliani a perpetual, worldwide, non-exclusive,
royalty-free and irrevocable license to use, modify and distribute it, and to license it under
other terms, proprietary ones included. The contribution is also published under the project's
license, the GNU AGPL version 3 or (at your option) any later version, like the rest of the
code.

A signed contributor agreement may be asked for larger contributions in the future.
