# third_party

Source this repository did not write, kept here because it had to be changed.

`third_party` rather than `vendor`: `cargo vendor` writes a directory called
`vendor` and replaces its whole contents, so a hand-patched crate living there
would be destroyed by a command somebody runs for an unrelated reason. Nothing
here is `cargo vendor` output — it is a small number of crates copied in on
purpose, and the name says that.

Each crate is wired in by a `[patch.crates-io]` entry in the workspace root, and
excluded from the workspace so that `cargo fmt --all`, `cargo clippy
--workspace` and `cargo test --workspace` keep meaning "code this repository
wrote".

---

## tiberius

| | |
|---|---|
| Upstream | <https://github.com/prisma/tiberius> |
| Version | 0.12.3 (published 2024-05-21; the last release there has been) |
| Upstream commit | `c34fab2e14c52ab74519d073d7a7b65bd023fc1a` |
| Copied from | the crates.io package, `~/.cargo/registry/src/index.crates.io-*/tiberius-0.12.3` |
| Licence | MIT or Apache-2.0, both included |

Removed from the copy, because none of it is compiled and all of it is about
running upstream's own CI: `.github/`, `docker/`, `docker-compose.yml`,
`flake.nix`, `flake.lock`, `.envrc`, `rust-toolchain`, `.cargo-ok`. Nothing else
was deleted, so `diff` against a fresh 0.12.3 shows exactly the patch below and
nothing else.

### Why it is here

Published 0.12.3 walks into `todo!()` while decoding four SQL Server types:
`geometry`, `geography` and `hierarchyid`, which travel as TDS type `Udt`, and
`sql_variant`, which travels as `SSVariant`. The `Udt` panic happens while
parsing `COLMETADATA` — before any row, so there is no point at which a caller
could look at the column and back out — and this workspace builds release with
`panic = "abort"`. Browsing an ordinary table that happened to have a
`geography` column therefore killed the whole application without a message.

Upstream has had no functional release since 2024, so there is nowhere newer to
point at.

### The patch

Everything below turns a panic into either a decoded value or a returned error.
No arm added or touched here can abort the process.

**Reading the three types**

- `src/tds/codec/type_info.rs` — new `TypeInfo::Udt(UdtInfo)` variant; `decode`
  reads `UDT_INFO` (MS-TDS 2.2.5.5.3) and keeps the CLR type name, and reads the
  four-byte length `sql_variant` carries. Both were `todo!("not yet implemented
  for {:?}", ty)`.
- `src/tds/codec/column_data/udt.rs` — **new**. Reads the UDT body, which is
  always PLP-encoded, and hands it over as `ColumnData::Binary`. What the bytes
  mean is the caller's decision; see `crates/drivers/mssql/src/udt.rs`.
- `src/tds/codec/column_data/variant.rs` — **new**. Decodes a `sql_variant`
  value into the `ColumnData` variant its own header says it is (MS-TDS
  2.2.5.5.2), including the property bytes each base type carries.
- `src/tds/numeric.rs` — `Numeric::decode` split so `decode_body` can be reached
  by the variant path, which knows the length from the variant header instead of
  reading it in front of the digits.
- `src/tds/codec/column_data.rs`, `.../column_data/var_len.rs` — dispatch to the
  two new decoders.

**Carrying the type name out to the caller**

- `src/row.rs` — `Column::udt_type_name()`. `ColumnType::Udt` says a column
  holds a CLR type but not which one, and every UDT value is an opaque byte
  string, so without the name a caller cannot tell a `geography` from a
  `hierarchyid`.
- `src/tds/codec/token/token_col_metadata.rs` — fills it in, and stops
  panicking in `null_value` and in `Display`.
- `src/tds/stream/query.rs` — used to build `Column` a second time by hand;
  now calls `TokenColMetaData::columns()` so there is one mapping to keep right.

**Refusing rather than panicking**

- `TypeInfo`/`VarLenContext` encode: sending a UDT or a `sql_variant` as a
  parameter now returns an error. There is no wire form for it and there never
  was; the `todo!()` there simply took the process down.

**Every other panic upstream left behind**

The four types above are what made this copy necessary. A second pass went
after the rest, on the principle that under `panic = "abort"` the failure mode
is the same whatever the cause: the window disappears, the user's unsaved work
goes with it, and nothing on screen or in a log says why. A returned error
costs one failed query.

On the read path:

- `src/tds/stream/token.rs` — an unrecognised token type was `panic!`, on the
  one path every token of every response goes through. It is worth being exact
  about what could reach it, because "some future server version" makes it
  sound remote and it is not: `TokenType::try_from` already rejects a byte that
  names no token, so the arm is reached only by a token this crate has a name
  for and no handler for, and there is **exactly one** — `ColInfo` (`0xA5`),
  which SQL Server sends in browse mode. Now a `Protocol` error naming the
  token.
- `src/tds/codec/column_data/int.rs` — `unimplemented!()` for an `Intn` whose
  received length is not 0, 1, 2, 4 or 8. Now a `Protocol` error naming both
  lengths, with a unit test that builds the bytes by hand. That is the only
  proof available, since no real server sends this.
- `src/tds/codec/token/token_col_metadata.rs` — four `unreachable!()`s in the
  `Display` impl, matching on lengths and type bytes that come straight off
  `COLMETADATA`. `Display::fmt` cannot return a `Protocol` error, so these
  print the unexpected value instead, the way the `Xml`/`Udt` arm beside them
  already did. Without this, the decoder returned a clean error for an
  oversized `Intn` while merely naming that column's type still aborted.

During connect, where a failure at least has an obvious cause:

- `src/tds/codec/pre_login.rs` — the server refusing the requested encryption
  level, and unrecognised pre-login tokens. `negotiated_encryption` returns
  `Result` now, which is why `client/connection.rs` grew a `?`.
- `src/tds/codec/login.rs` and `src/tds/codec/token/token_feature_ext_ack.rs` —
  unrecognised login feature extensions.
- `src/client/config.rs` — `trust_cert` and `trust_cert_ca` used together. Both
  return `crate::Result<()>` now. This changes the vendored crate's **public
  API**, deliberately: the pair is not only a programmer error, it is reachable
  from `Config::from_ado_string`, so a connection string a user typed could
  abort the process. Nothing in this workspace calls either method, so the only
  callers updated were inside the crate and its examples.

Writing, which this driver does not do through these paths, fixed anyway
because the argument does not depend on who walks the path:

- `src/tds/codec/column_data.rs` — `todo!()` when a `Numeric` parameter's scale
  disagrees with the server's.
- `src/tds/codec/rpc_request.rs` — `todo!()` for calling a procedure by name
  rather than by id.

### What still panics, and why each one stays

Nothing below can be reached by anything a server sends. Each was checked
rather than assumed: "unreachable" is a claim about the whole call graph, and
the compiler does not check it for you.

- `src/tds/numeric.rs` — `decode_d128` matches on `buf.len()`. Its two callers
  pass a local `[0u8; 12]` and `[0u8; 16]`; the server-supplied length was
  already validated by the arm above them, which returns a `Protocol` error.
- `src/tds/codec/token/token_done.rs` — matches on `done_row_count_bytes()`, a
  function that returns literally 4 or 8.
- `src/error.rs` — `From<Infallible>`. Unreachable by the type system: there is
  no value to convert.
- `src/client/connection.rs`, three sites — two are an auth library returning
  `None` where its own contract says `Some`; one is `transport.into_inner()`
  being anything but `Raw` before the handshake that creates the other variant.
- `src/tds/time.rs` — an encode path matching on `Time::len()`, which returns
  3, 4 or 5 and a `Protocol` error for anything else.

`src/sql_read_bytes.rs` has three `todo!()`s in the `SqlReadBytes` methods of a
`#[cfg(test)]` helper reader. Those are compiled by our own build now, because
the `int.rs` test above uses that helper — but `debug_buffer`, `context` and
`context_mut` are not called by the decode paths a test drives, and a test
binary that panics is a failed test rather than a lost session.

There is also a `panic!` inside a `#[cfg(test)]` assertion in
`column_data.rs`, which is what a failing test is supposed to do.

### Upgrading

There is nothing to upgrade to today. If there ever is: copy the new version in
the same way, then re-apply the list above. `git log -- third_party/tiberius`
is the history of this patch.
