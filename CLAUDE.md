# Notes for Claude

- Commit as the repository owner, not as Claude. Before the first commit in a session run:

  ```sh
  git config user.name "v0-0x"
  git config user.email "139093075+v0-0x@users.noreply.github.com"
  ```

- The PKGBUILD's `check()` runs `cargo test --frozen --release`, so keep tests offline and
  `Cargo.lock` committed and up to date.
