# valart

Compact radix trees for the **Hash** and **Set** values of
[OxidArt](https://crates.io/crates/oxidart) / [RadixOx](https://github.com/JoshuaBOUCHAT/radixox):
one small tree per container, nodes in a shared slab, entries stored inline in 128-byte nodes
with an overflow block for wide fan-out. Binary-safe fields and members.

This crate is an internal building block of OxidArt; its API follows OxidArt's needs
(single-threaded, `SharedByte` values, copy-on-write images for snapshots) and is not meant
as a general-purpose collection.

License: MIT
