# vmlab-disk design: imago vs own qcow2

Decision: own implementation, written from the qcow2 spec.

## Spike: can imago carry internal snapshots and vmstate?

No. Evidence from reading imago 0.2.6 (MIT) from crates.io:

- The qcow2 header fields `nb_snapshots` and `snapshots_offset` are parsed
  read-only. `src/qcow2/metadata.rs` wraps them in `check_field!` with the
  upstream comment `// TODO: Should be mutable`. There is no code anywhere in
  the crate that writes a snapshot table entry, allocates a snapshot L1 table,
  or adjusts refcounts for shared clusters.
- The public API surface (`src/format/access.rs` and its sync wrappers) has
  read/write/discard/resize/mapping operations only. There is no
  snapshot-create/apply/delete entry point at all.
- "vmstate" does not appear anywhere in the crate: no matches for `vmstate`,
  `vm_state`, or reading a snapshot's L1. imago cannot even *read* a snapshot
  view, let alone write one.

Extending it cleanly from outside is not possible: snapshot writes need
mutable header fields, refcount updates against the shared-cluster allocator,
and L1/L2 table writes — all private to imago's `Qcow2` driver (its refcount,
allocation and cache modules expose nothing public). We would have to fork
imago and grow a second snapshot engine inside its async storage-trait
architecture, and then still answer for vmstate ourselves. The spec's deciding
factor (how cleanly imago takes snapshot-table and vmstate writes) answers
itself: it does not, at any level.

imago also pulls a large dependency surface (async infrastructure, storage
driver traits) that this crate does not otherwise need.

## Own implementation: grounding

Written from `docs/interop/qcow2.txt` (the spec text in QEMU's tree, QEMU 11.2
rendering), with QEMU used black-box to pin down details the spec leaves open.
Observations from a throwaway VM (`savevm` via QMP on a 10 MiB qcow2, 64 KiB
clusters, QEMU 11.1.2):

- The vmstate is stored like guest data starting at the first L1 entry that
  describes no regular guest cluster: for a 10 MiB disk (one L1 entry of a
  512 MiB span) the vmstate occupies L1 entry 1, i.e. guest offset
  `ceil(size / l2_span) * l2_span`. QEMU grows the *active* L1 to cover it
  (l1_size 1 → 2).
- After `savevm`, QEMU punches the vmstate out of the active layer: the active
  L1's vmstate entry is COWed to a fresh L2 whose entries are zero-flag
  descriptors (`0x1`), dropping the active references to the vmstate clusters
  while the snapshot's L1 keeps them.
- After `qemu-img snapshot -a`, the active L1 points at the snapshot's L2
  (shared, copied bit cleared); the vmstate is readable through the active
  layer until the next snapshot replaces it.
- Snapshot table entries carry `extra_data_size = 24` (vm_state_size_large,
  disk size, icount = 0); ids are increasing decimal strings starting at "1".
- `qemu-img commit -d` copies the top's allocated clusters into its backing
  and leaves the top image untouched.
- `qemu-img convert -l snapshot.name=… -B backing` writes only the clusters
  allocated in the source's own layer into a dest backed by `backing`.

These behaviours are reproduced in `tests/s2_qemuimg.rs` against qemu-img
itself, which is the acceptance seam (S2).

## Consequences

- The `DiskImage` trait in `lib.rs` is unchanged: qcow2 is one implementation
  behind it, as D8 requires.
- Writes are never compressed (deflate read support only, per the brief);
  clusters we allocate are plain, zero-flag descriptors carry the zero case.
- The refcount engine supports arbitrary `refcount_order` 0–6 so images made
  with `qemu-img create -o refcount_bits=1…64` round-trip.
