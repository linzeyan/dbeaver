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

### Known panics still in this copy

Left alone deliberately — they are upstream's, they are not on any path this
patch touches, and each one changed makes the diff harder to re-apply. They are
listed because the four that were fixed were not the only ones, and somebody
looking for the next abort should not have to find this out by suffering it.

On the read path, and so the ones that matter:

- `src/tds/stream/token.rs:250` — an unrecognised token type is `panic!`. This
  is **worse than any of the four fixed above**: `COLMETADATA` at least needed a
  particular column type to reach it, whereas this is every token of every
  response, and a token a future server version adds takes the process with it.
- `src/tds/codec/column_data/int.rs:18` — `unimplemented!()` for an `Intn` whose
  received length is not 0, 1, 2, 4 or 8.

During connect, where a failure at least has an obvious cause:

- `src/tds/codec/pre_login.rs:73, 208, 243` — the server refusing the requested
  encryption level, and unrecognised pre-login tokens.
- `src/tds/codec/login.rs:526, 540, 550` and
  `src/tds/codec/token/token_feature_ext_ack.rs:43, 48` — unrecognised login
  feature extensions.
- `src/client/config.rs:138, 154` — `trust_cert` and `trust_cert_ca` together.

Writing, which this driver does not do through these paths:

- `src/tds/codec/column_data.rs:683` — `todo!()` when a `Numeric` parameter's
  scale disagrees with the server's.
- `src/tds/codec/rpc_request.rs:109` — `todo!()` for calling a procedure by name
  rather than by id.

And `src/sql_read_bytes.rs:387, 391, 395` are three `todo!()`s inside
`#[cfg(test)]` helpers, which nothing outside upstream's own tests compiles.

There are also a dozen `unreachable!()`s, mostly in `Encode` and `Display`
impls; the same argument applies to all of them.

### Upgrading

There is nothing to upgrade to today. If there ever is: copy the new version in
the same way, then re-apply the list above. `git log -- third_party/tiberius`
is the history of this patch.
