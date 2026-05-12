//! Library surface for the `classic` CLI binary. Exposes the
//! reusable pieces (TOML schema parser, submit subcommand handler)
//! so integration tests under `tests/` can exercise them without
//! shelling out to the binary.

pub mod group_toml;
pub mod submit;
