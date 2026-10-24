# Vendored yrs 0.28.0 — local security patch

This is an unmodified copy of `yrs` 0.28.0 (from crates.io) with one security fix applied,
pulled in via `[patch.crates-io]` in `server/Cargo.toml` and `server/fuzz/Cargo.toml`.

The exact diff against pristine yrs 0.28.0 is `0001-cap-decode-counts.patch` in this directory
(`git format-patch` form; `git am` or `git apply` it on a yrs checkout).

## The change

CRDT decoders (`Update`, `IdSet`/`IdRanges`, `StateVector`) read an element count as a varint
and then reserve/loop that many times. `try_reserve` was already used, but it only turns an
*allocator failure* into an error; under memory overcommit (Linux/macOS default) the kernel
honours a multi-gigabyte reservation and the process is OOM-killed when the pages are touched.
A ~18-byte crafted sync message could make the server attempt tens of GB.

Fix: `Decoder` gains `remaining()` (bytes left to read; `Some` for the v1 byte decoder, `None`
otherwise) and `check_len(len)`, which rejects a declared count larger than the bytes still
available — every element costs at least one wire byte, so a larger count is always malformed.
`check_len` is called before each reserve/loop in `update.rs`, `id_set.rs` and `state_vector.rs`.
v2 decoders return `None` (counts there are spread across sub-streams), so their behaviour is
unchanged; this crate only ever puts v1 on the wire.

Diff is limited to:
- `src/updates/decoder.rs` — `remaining()` + `check_len()` on the trait, `remaining()` for `DecoderV1`.
- `src/update.rs`, `src/id_set.rs`, `src/state_vector.rs` — a `check_len(...)?` before each count use.

## Manifest addition

`Cargo.toml` additionally carries a `[lints.rust] warnings = "allow"` table that pristine yrs
0.28.0 does not have. Cargo caps the lints of registry crates but not of path dependencies, and
this copy is pulled in by path, so without it a current toolchain prints ~60 rustc warnings
from yrs on every build of the server, its tests and the fuzz targets. The table changes no
behaviour and is not part of `0001-cap-decode-counts.patch`; it goes away with the directory.

## Upstream

This is the same problem as y-crdt/y-crdt#675 ("Decoding reserves what the input claims"),
which is open and matches this fix's approach (cap each count by the bytes remaining). We
confirmed it there and posted this diff:
https://github.com/y-crdt/y-crdt/issues/675#issuecomment-6232787802

Scope vs #675: this diff caps the client / block / range / state-vector counts only. It does
**not** cap the string and `Any`-value reservations, nor bound `Any` nesting depth (a separate
stack-overflow path) that #675 also lists; our fuzzing is clean because the reachable large
allocations were the counts capped here. Related and already in flight upstream: #679/#685
(client ids >= 2^53, our F-31, handled by catch_unwind + release build here), #680/#684
(clock+len overflow), #690 (v2 decoder panics).

When upstream ships a fix (likely wider than this one), drop `third_party/yrs` and the two
`[patch.crates-io]` stanzas and bump the `yrs` version. Until then we run this vendored copy to
confirm it rejects nothing legitimate.
