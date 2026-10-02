# Fuzzing

This is an independent cargo-fuzz workspace. Its dependencies, lockfile and
release profile do not affect the library or its Rust 1.91 MSRV. All harness code
lives here and uses public library APIs; no fuzz-specific hooks or configuration
are added to the library.

## Run

Requires nightly Rust and a C++ compiler (for libFuzzer). Do not change the
project's default toolchain:

```sh
rustup toolchain install nightly
cargo binstall cargo-fuzz

# From the repository root. The first directory is writable; seeds stay unchanged.
mkdir -p fuzz/corpus/compression
cargo +nightly fuzz run compression fuzz/corpus/compression fuzz/seeds/compression -- \
  -max_total_time=300 -timeout=10 -rss_limit_mb=1024 -max_len=262144
```

Repeat for `filesystem`, `xattrs`, `devices`, and `read_contract`. ASan is enabled
by default. The fuzz release profile enables overflow checks and debug assertions.
Keep the generated corpus between runs; `fuzz/corpus`, `artifacts`, `coverage`
and `target` are ignored by Git. The small `seeds` directory is checked in.

## Targets and input formats

| Target | Input and checks |
| --- | --- |
| `filesystem` | Raw image; superblock/inodes, byte paths, bounded directory walking, symlinks, xattrs and file streams. Compare sync/async metadata, walks and streams. |
| `compression` | Image with Full/Compact indexes, inline payloads, fragments or modern extents. Compare bounded sequential streams with different read sizes, including partial decoding and sparse high physical addresses. |
| `xattrs` | Image; inline/shared entries, namespaces, opaque names/values and plain/packed/compressed prefixes. Compare complete byte maps. |
| `devices` | Up to 8192 bytes of primary image, then up to 2048 bytes of device 1, then device 2. Boot byte 10 modulo 3 selects the number of supplied devices. Exercise indexed chunks, flat/inline/compressed data, masks, holes, device ranges and high addresses. |
| `read_contract` | Byte 0 selects a fixed valid plain/four-codec/metabox image. Remaining bytes generate up to 32 `(u16 buffer_size, u8 failure_point)` operations via `Arbitrary`. Check against known file bytes, not just the other executor. Also retry failed directory block loads. |

For image targets, unused boot bytes 16..24 supply an inode ID (little-endian).
For filesystem path lookup, byte 31 modulo 65 supplies a length and bytes starting
at 32 supply the byte path. These controls are outside EROFS metadata.

In `compression` and `xattrs`, boot byte 9 bit 0 selects a fixed valid outer
envelope (512-byte blocks, inode 1 at 2080) to avoid spending all iterations on
superblock rejection. Compression byte 10 bit 0 selects Full/Compact. The raw
filesystem target never repairs metadata. Component fixtures need not constitute
an entirely valid filesystem: for example, the large-xattr fixture exercises the
attribute body directly rather than traversing its directory.

The backend exposes the input bytes at offsets 0, `1 << 40` and `1 << 48`;
unrepresented gaps fail reads. This tests wide byte/block addresses without
allocating huge images. Async exact-read failures may modify the destination.
No network, mmap, native encoder or `mkfs.erofs` is used in the hot loop.
The `metabox-*` seeds cover all four codecs and were checked with `fsck.erofs`;
boot bytes select the full high-bit file NID. Replay also verifies file bytes
and xattrs against known source values. The operation target includes these
images to inject failures while loading metabox-backed metadata.

## Oracles and budgets

- Invalid images returning errors are expected; panics are not caught.
- Short-read partitioning may differ. Compare concatenated bytes, not each call's size.
- Nonempty reads cannot report EOF before the declared size on a successful stream.
- The operation target verifies cache hits/empty reads/EOF do not perform I/O,
  and failed file or directory block loads do not skip data after recovery.
- Each executor has a 1024-request / 64 MiB backend budget. Budget exhaustion
  ends that comparison; it is not a library error or evidence of corruption.
- Streams stop after 64 KiB, walks after 16 entries/depth 8, symlink reads at
  4 KiB. Inputs above 1 MiB are skipped. On-disk sizes/addresses are **not** clamped
  before parsing (except the explicitly selected fixed-envelope mode).
- These harness budgets prevent unbounded caller work; libFuzzer's timeout/RSS
  limits still monitor work inside individual library calls. Triage resource
  failures to distinguish real denial-of-service bugs from harness overhead.

Sync/async agreement cannot detect a shared parser bug. Seeds reuse the small
fixtures and independently encoded codec samples from `erofs/src/{tests.rs,
compression/tests.rs,xattr/tests.rs,devices/tests.rs}`. Harness tests verify that
codec seeds reach decoding, device streams have expected bytes, and xattr
seeds return expected names/values. Keep ordinary reference-image and mmap tests;
never fuzz by modifying an actively mapped file in violation of its safety contract.

## Replay, minimize, coverage

```sh
# Harness checks, including replay of every checked-in seed.
cargo +nightly test --manifest-path fuzz/Cargo.toml --lib --locked
cargo +nightly test --manifest-path fuzz/Cargo.toml --lib --no-default-features --locked

# Also exercise the alloc-only library configuration on the host.
cargo +nightly fuzz run compression --no-default-features --features lz4,deflate,zstd -- -max_total_time=60

cargo +nightly fuzz run compression fuzz/artifacts/compression/crash-REPLACE
cargo +nightly fuzz tmin compression fuzz/artifacts/compression/crash-REPLACE
cargo +nightly fuzz cmin compression
rustup component add llvm-tools-preview --toolchain nightly
cargo +nightly fuzz coverage compression
```

Host fuzzing of the alloc-only library does not establish bare-metal portability.
Promote minimized failures to ordinary regression tests so stable CI catches them
without needing nightly or libFuzzer. The public file API has no seek, so this
harness does not jump to arbitrarily large logical offsets. Existing library
unit tests retain that coverage; the wide-extent seed exercises its leading hole,
not its compressed payload beyond 4 GiB. Do not use a failed reference image as an
oracle merely because it was generated by `mkfs.erofs`.

The fuzz workflow runs every target for 30 seconds on PRs and 10 minutes on
manual runs. It retains corpora via cache and uploads failing artifacts
and the corresponding corpus. Normal stable workspace CI remains separate.
