# Spike: verified, resumable transfer with bao-tree

Written by the lead from the spike agent's report.

## Verdict: go with caveats

bao-tree 0.17 (`default-features = false` plus `validate`) builds unchanged for `wasm32-unknown-unknown`. One transport-agnostic library verifies everything it receives, resumes after a crash requesting only missing units, rejects and re-fetches corrupted units, and checks the final hash, natively and in a browser worker against OPFS.

Caveats:

1. **OPFS call overhead.** Chrome's sync access handle costs about 150-700 us per call regardless of size. Unbatched, the browser transfer ran at 8-12 MB/s; with one read and one write per unit it runs at 53-70 MB/s. Batching per unit is mandatory.
2. **Old dependencies via `validate`.** It pulls in genawaiter 0.99 (syn 1, unmaintained proc-macro-error). It is used only for the repair scan, which can be reimplemented in about 30 lines.
3. **Pre-1.0 crate.** Pin it and keep it behind our own module.

## Proven by passing tests

Native (`cargo test --release -- --nocapture`, 12 of 12):

- Edge sizes from 0 bytes to 3 MB transfer exactly; the bao root equals plain `blake3::hash` of the file.
- Seven cuts in a row, each followed by a disconnect or a crash: every session requests at most the missing units.
- Bit flips in transit are rejected, re-requested and recovered; a rejected unit never touches disk.
- Simulated power loss with torn writes, 20 rounds: every unit the journal claims is byte-correct after restart.
- Host disk rot after receipt, a lost journal, and guest disk rot are each detected and repaired or reported.
- The incremental outboard is byte-identical to bao-tree's across 16 sizes, including header patches.
- 200 MiB through real files with fsync, three crashes and two bit flips: final BLAKE3 equals the root.

Browser (`npm --prefix browser test`, Chrome 157 headless, module Worker, scalar and simd128):

- 64 MB in-memory benchmark, decoded bytes equal.
- 100 MB OPFS to OPFS through the same Sender/Receiver code with 3 crashes and 2 bit flips.

## Not proven

- A real RTCDataChannel or str0m channel, and `bufferedAmount` backpressure.
- Firefox and Safari.
- Durability ordering in the browser: OPFS `flush()` measured about 0 ms, so it may not be a durability barrier. Not blocking, because the receiver is the native host.
- `cargo audit` (advisory database fetch failed).
- Where real muxers back-patch their headers; the S3 multipart mapping.

## Measurements

Native, release, single thread:

| Operation | MB/s |
|---|---|
| `blake3::hash` | 4440-4550 |
| bao outboard, 16 KiB groups | 3760-3900 |
| Encode (validated) | 1060-1380 |
| Decode (verify + write) | 1420-2250 |
| 200 MB file to file, fsync every 8 MiB, with crashes | 169-174 |

Browser, wasm32 in a Worker:

| Operation | Scalar MB/s | simd128 MB/s |
|---|---|---|
| Hash | 680-870 | 1230-1550 |
| Decode | 640-780 | 1210-1440 |
| Record to OPFS (1 MiB writes + hash) | 100-140 | 130-150 |
| Full protocol OPFS to OPFS with crashes | 53-58 | 66-70 |

Wire overhead is 0.52% (1 MiB units, 64 KiB frames). bao-tree, its dependencies and our code add at most about 200 KB raw, 70 KB gzip to the WASM.

## Framing

One message per data channel message, little-endian, hand-rolled:

```
REQUEST 0x01 | root[32] | n:u32 | n x (start_unit:u64, end_unit:u64)   receiver -> sender
DATA    0x02 | root[32] | unit:u64 | offset:u32 | total:u32 | payload  sender -> receiver
DONE    0x03 | root[32]                                                 receiver -> sender
ERROR   0x04 | root[32] | code:u8                                       sender -> receiver
```

The receiver drives, keeping 8 units in flight; resuming is re-sending REQUESTs for missing units. DONE means "verified on my disk" and is the guest's cue to delete its copy. Root and size come from the take manifest.

## Sizes

- Chunk group 16 KiB (outboard is 0.4% of the file).
- Frame 64 KiB (under Chrome's 256 KiB limit, and small enough not to block live audio).
- Unit 1 MiB: the unit of request, verification, commit and resume.
- S3 part 8 MiB: any power-of-two part of at least 5 MiB is an exact bao subtree, so the host can verify each part before upload. S3 cannot check BLAKE3, so add its own CRC32C per part.

## Crash safety on the receiver

Three files per blob: preallocated data, a zero-filled pre-order outboard (zero pairs never verify), and an append-only journal of 16-byte records. Commit order: sync data, sync outboard, append records, sync journal. On open the journal is truncated to its valid prefix. A crash loses at most the units since the last commit (default 8 MiB). The whole file is hashed once at the end; a mismatch triggers a rescan and re-request of only the bad units.

## Hashing while recording

The root and outboard can be computed incrementally with O(log n) memory and nothing re-read at the end of a take. Containers back-patch headers (WAV sizes, FLAC STREAMINFO, MP4 mdat size), so the hasher keeps a patchable 64 KiB prefix in memory and re-hashes only that subtree at finish. Since audio becomes FLAC after the take, the hasher belongs on the FLAC encoder's output.

## iroh-blobs: no

It depends directly on the iroh QUIC stack (about 420 crates) and assumes an iroh connection. Use bao-tree directly and treat iroh-blobs as a reference design.

## Recommendations

1. bao-tree, pinned, behind a castcorder transfer module shaped like this spike: sans-IO Sender and Receiver, a `Storage` trait, a `Channel` trait. Same code for the native and WASM cores.
2. 16 KiB groups, 1 MiB units, 64 KiB frames, 8 MiB S3 parts; record the geometry, root and size in the take manifest.
3. Batch I/O per unit on every backend, and run the browser side inside the OPFS worker.
4. Build WASM with simd128 and blake3's `wasm32_simd` feature.
5. Consider dropping `validate`.
6. Next: a real data channel through str0m with backpressure, measured alongside live audio; a Firefox run; OPFS flush durability.

Fallback if bao-tree is ever abandoned: fixed 1 MiB chunks with a BLAKE3 hash list under a custom root. Viable but loses standard roots and range proofs.
