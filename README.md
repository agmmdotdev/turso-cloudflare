# Turso WASM + Durable Objects + lazy R2 storage

Tested on 11 November 2026 in a Linux cloud workspace under Miniflare/workerd. **84/84 checks passed: 24 regression checks, 40 large-database checks, 13 reader checks and 7 owner-control checks.** The largest database contains **1,007,500 rows in 1,075,445,760 physical bytes**. No Cloudflare production deployment was performed.

Turso parses and executes every application SQL statement. Native Durable Object SQLite is only the byte-storage backend for local file chunks and metadata. The Turso engine source was not modified; the custom Rust/WASM bridge and JavaScript storage adapter implement the integration.

The local single-merchant load steps, measured latency, and reproduction command are in [STEP-LOAD.md](STEP-LOAD.md). The matched ten-minute checkpoint comparison is in [CHECKPOINT-DIAGNOSTIC.md](CHECKPOINT-DIAGNOSTIC.md).

The sustained checkout benchmark and deployment steps are in [MERCHANT-LOAD.md](MERCHANT-LOAD.md). The repeated-checkpoint fix and its separate measurements are in [PAGED-CHECKPOINTS.md](PAGED-CHECKPOINTS.md).

## Run the experiment

Node.js 24 and Python 3 with its standard SQLite module are required. A prebuilt WASM is included.

```sh
npm ci
npm test
npm run test:large
npm run test:readers
```

The large run generates actual indexed SQLite-format databases targeting 32, 256 and 1024 MiB, seeds them into local R2, and opens, queries, updates, checkpoints and restores them using Turso WASM inside a DO. Generation used bounded Python batches and took about 13 seconds in this environment. These are populated databases containing products, categories, unique SKU and category indexes, Burmese text, numeric fields and a 900-byte deterministic payload per product. They are not sparse files or fake large-size headers.

Expect several gigabytes of scratch disk use. `npm test` resets only `.state`; the large test resets only `.large-state`. Generated datasets remain in `.large-data`. The databases are regenerated rather than included in the ZIP. `npm run test:large:reuse` reuses existing immutable seed objects after checking that all segment objects and their sizes exist. Final validation reused seeds; seed-upload timing is consequently zero in those records and is not a performance result.

For a smaller run:

```sh
python3 generate-data.py
LARGE_SIZES=32 node large-test.mjs
```

To rebuild the WASM, install Rust 1.99.1 and its WASI target, then use the supplied build command:

```sh
rustup toolchain install 1.99.1 --profile minimal --target wasm32-wasip1
npm run build
```

The source setup checks out Turso tag v0.8.3 at commit `607bff9fd0e547a25a97b3bf06e83bcc5d99d53c` into a sibling `turso-source` folder and checks its commit before building. It does not overwrite another checkout. Cargo builds an optimized dev profile, with the WASM linker maximum set to **64 MiB** in `rust/.cargo/config.toml`.

## What changed from the first experiment

- Cold opening loads the manifest and fetches database pages as needed; it does not hydrate the whole R2 snapshot.
- Turso file completions, database open/connect and SQL stepping suspend and resume through the custom ABI.
- Reads check staged changes, durable DO chunks, an LRU block cache, and then R2 byte ranges.
- R2 reads use a BYOB reader and reuse evicted cache buffers. Cached page views copy directly into WASM rather than allocating a new page-sized JavaScript buffer for each read.
- Writes are staged privately across asynchronous reads. The adapter atomically writes the staged changes to DO SQLite, awaits `storage.sync()`, and only then returns success. A synchronous storage transaction is never held across a network await.
- Staged file writes are limited to **8 MiB per HTTP operation**. Oversized operations fail and are recovered without persisting their partial changes. SQL results are limited to 512 rows.
- Snapshots contain immutable **4 MiB segments** plus a manifest. Checkpointing reuses unchanged segments and composes changed ones with one reusable 4 MiB buffer. WAL and MVCC logical-log sidecars are included when present.
- Snapshot upload finishes before one DO storage transaction publishes the manifest and removes local chunks. Failure before publication leaves the previous manifest and durable data usable.

The snapshot format records SHA-256 values when segments are written. Lazy range reads validate returned length and object presence; they do not recompute a whole-segment hash for every partial read. End-to-end range corruption detection would need block hashes or verified complete-segment reads.

## One owner and pinned read-only DOs

`npm run test:readers` repeats the 1 GB test with one owner DO, four distinct reader DOs, immutable checkpoint manifests carrying a committed version, and 30 atomic checkout transactions per case. Each checkout decrements a stocked product and inserts an order in one transaction. Reader SQL is restricted to SELECT statements and mutations are rejected. A caller requesting a version newer than the reader's pinned snapshot is routed to the owner. Pinned readers continue serving the original version until explicitly repinned. Tested totals match the native reference, and all checkout orders and stock changes are verified.

| Scan mode | Readers | Scan wall time | Checkout p50 | Checkout p95 | Reader R2 transfer | Reader GETs |
|---|---:|---:|---:|---:|---:|---:|
| single | 0 | 20.15 s | 20004.1 ms | 20048.1 ms | 0.0 MiB | 0 |
| single | 1 | 20.28 s | 20155.3 ms | 20182.2 ms | 1025.8 MiB | 16,412 |
| single | 2 | 20.98 s | 20845.0 ms | 20879.8 ms | 1026.0 MiB | 16,416 |
| single | 4 | 17.82 s | 6139.6 ms | 6205.8 ms | 1026.5 MiB | 16,424 |
| sliced | 0 | 22.95 s | 112.7 ms | 213.7 ms | 0.0 MiB | 0 |
| sliced | 1 | 21.62 s | 116.6 ms | 201.6 ms | 1025.8 MiB | 16,412 |
| sliced | 2 | 21.46 s | 88.0 ms | 238.5 ms | 1024.8 MiB | 16,397 |
| sliced | 4 | 15.88 s | 201.8 ms | 499.9 ms | 1025.1 MiB | 16,402 |

Single mode runs one aggregate query per DO and submits all 30 checkouts during the scan. Sliced mode splits scans into 8,192-row range queries, sends 30 checkouts 200 ms apart, and sums the range results at the client. The owner-only scan shares the owner's request queue; the owner-sliced control is a separate run with the same 1 GB dataset. Readers pin a snapshot in their own DO storage and lazily read its R2 segments. Reader bytes are per-request deltas, so they remain meaningful if Miniflare recreates an actor.

One large SELECT can occupy the local workerd scheduler for seconds even when a different DO owns the checkout. Splitting the scan gives waiting requests scheduling opportunities. The owner-sliced control shows this can improve checkout latency without adding reader DOs, but its separate range requests are not protected by one query snapshot if writes touch later ranges. Readers pinned to one immutable manifest have a consistent version across all range queries. Miniflare's scheduler and local emulated R2 must not be taken as Cloudflare production parallel throughput. The scheme has one writer; it does not solve concurrent commits by independent writer DOs. R2 segments are immutable, while acknowledged changes since the latest checkpoint are still durable in the owner DO. Checkpoints still block owner work and reader refresh is manual in this prototype.



## Query results

All numbers below are measured local end-to-end timings, not predictions for production R2. The main comparison uses a 16 MiB remote cache, 64 KiB R2 reads and an approximately 4 MiB Turso page-cache setting. Cold query timing includes WASM instance construction, database open and the first indexed lookup. Warm queries perform the same lookup 50 times; random tests use 300 deterministic lookups across the database.

| Actual database size | Product rows | Cold indexed query | Cold R2 download / GETs | Warm lookup p50 | Full data scan |
|---|---:|---:|---:|---:|---:|
| 33.00 MiB | 32,500 | 178.1 ms | 260 KiB / 5 | 4.46 ms | 0.81 s |
| 257.02 MiB | 252,500 | 155.4 ms | 212 KiB / 4 | 7.53 ms | 7.09 s |
| 1025.62 MiB | 1,007,500 | 148.2 ms | 320 KiB / 5 | 4.75 ms | 23.45 s |

The full scan calculates count, stock sum, price sum and payload length sum and compares all four with a native SQLite reference. It traverses product records instead of merely counting index entries. It does not hash every payload byte.

## Memory results

| Actual database size | Remote cache at scan end | WASM linear memory | Sampled JS heap + backing-store peak |
|---|---:|---:|---:|
| 33.00 MiB | 16.0 MiB | 10.81 MiB | 21.64 MiB |
| 257.02 MiB | 16.0 MiB | 10.88 MiB | 24.94 MiB |
| 1025.62 MiB | 16.0 MiB | 10.88 MiB | 35.20 MiB |

The WASM maximum is enforced by the module. The remote cache and staging buffers have separate explicit byte budgets. Rust allocator live/peak counts are also recorded in the raw results. Turso's own page-cache size is a soft limit, so measured memory is necessary in addition to configuration.

JavaScript memory is sampled through the inspector on the application Worker isolate. WASM linear memory is reported separately. These are sampled components, not a complete native-runtime peak or a proof that all production memory accounting stays below Cloudflare's limit. Development DOs may be reconstructed between requests; scan and checkpoint counters are therefore returned within their operations, and post-request actor identities are retained in the raw results.

## Snapshot and recovery results

A small update to the largest database checkpointed by uploading **5,898,240 bytes (5.62 MiB)** and reusing **255** segments, in **174.1 ms** locally.

To test full snapshot construction rather than just incremental reuse, the harness forced a rewrite of all **1,075,445,760 bytes** into **257** R2 objects. The rewrite took **34.14 seconds**, used one **4 MiB** composition buffer, and then restored the snapshot into a fresh runtime. A full restored scan passed in **23.88 seconds**.

The sampled JS heap/backing-store peak through that phase was **89.00 MiB**, with **9.69 MiB** of WASM linear memory reported separately. See the memory-measurement limits above.

Recovery checks cover runtime replacement, uncommitted transactions, failures inside file writes, response loss after durable commit, R2 read errors and short reads, upload failure, manifest-publication failure, and a remote read failure after earlier WAL writes have been staged. The last case confirms that asynchronous I/O did not weaken the HTTP operation's durability boundary. One hundred concurrent updates to a row in the largest database survived restart.

## R2 block-size comparison on the largest database

Each row includes a cold indexed lookup and 150 deterministic random lookups. Larger blocks reduce request count but download more unrelated data; the best size depends on the workload and real R2 latency.

| Cache budget | Read block | R2 GETs | Downloaded | Random lookup p50 | p95 |
|---|---:|---:|---:|---:|---:|
| 16 MiB | 4 KiB | 476 | 1.86 MiB | 6.29 ms | 12.22 ms |
| 16 MiB | 64 KiB | 410 | 25.62 MiB | 5.66 ms | 8.42 ms |
| 16 MiB | 256 KiB | 325 | 81.12 MiB | 5.43 ms | 8.57 ms |
| 32 MiB | 4 KiB | 475 | 1.86 MiB | 6.50 ms | 12.02 ms |
| 32 MiB | 64 KiB | 410 | 25.62 MiB | 5.94 ms | 10.37 ms |
| 32 MiB | 256 KiB | 324 | 80.88 MiB | 6.02 ms | 10.22 ms |

For scans, small blocks imply many more remote requests. A production adapter should investigate workload-aware read-ahead rather than treating one block size as universally best.

## Injected storage latency

These scenarios add the stated delay before each R2 range request. They are controlled sensitivity tests, not measurements of the live R2 network.

| Added delay per read | Cold query | Random lookup p50 | p95 |
|---|---:|---:|---:|
| 5 ms | 53.5 ms | 22.8 ms | 58.6 ms |
| 20 ms | 118.5 ms | 67.1 ms | 93.6 ms |

## Scope and remaining limits

- One DO still owns and executes each database on one thread. Lazy reads address database size; they do not distribute query CPU or provide shared writers across DOs.
- The large datasets start in SQLite WAL-compatible format. The separate regression suite tests Turso MVCC conflict detection, independent transactions, logical-log recovery and R2 restore; large MVCC-version histories were not stress tested.
- Workerd/Miniflare uses local storage. Real R2 latency, CPU billing, deployed startup limits, operating memory accounting and production throughput remain unmeasured.
- Checkpoints are explicitly triggered and serialized with SQL operations. Production would need automatic checkpoint scheduling, WAL growth limits and measures for long checkpoint pauses.
- Failed uploads and older generations are retained. Garbage collection must preserve segments referenced by every retained manifest.
- The prototype has no application authentication or complete connection/session lifecycle. Errors discard that DO's WASM instance and connections.
- The fault checks establish the modeled behavior tested here, not an exhaustive randomized crash-consistency proof.

## Versions and files

Node `v24.22.1`, Miniflare `4.20260730.0`, workerd `1.20260730.1`, Rust 1.99.1. The prebuilt WASM is **16,664,430 bytes**, approximately **4.24 MiB compressed**. This is not a verified deployed Worker bundle size.

`worker.mjs` implements storage, snapshots and reader routing; `wasi.mjs` supplies minimal WASI host functions; `rust/src/lib.rs` implements the resumable ABI and Turso I/O traits. `test.mjs`, `large-test.mjs` and `reader-test.mjs` are the harnesses. `generate-data.py` creates the populated reference databases. `results-summary.json` is the compact measured result; the full result JSON files retain per-test evidence. `make-report.py` regenerates this README and summary and checks the latest tested Worker module hashes and tested WASM hash against the delivered files. The reader benchmarks retain their original source hash.

`wrangler.jsonc` describes bindings for a possible future deployment; no account deployment was performed. The original browser-package compatibility probe remains included and records its expected `Worker is not defined` failure.

## Complete check list

| Suite | Check | Result |
|---|---|---|
| Small | Turso WASM executes SQL inside a SQLite-backed Durable Object | PASS |
| Small | DDL, index, Unicode, joins and aggregation | PASS |
| Small | Explicit multi-statement transaction rollback | PASS |
| Small | Constraint failure rolls back durable adapter writes | PASS |
| Small | Recreate WASM instance and recover durable WAL without R2 | PASS |
| Small | Dispose and recreate entire Miniflare runtime with persisted DO storage | PASS |
| Small | Injected failure before DO durable commit leaves no partial transaction | PASS |
| Small | Lost response after durable commit still recovers the whole transaction | PASS |
| Small | Failure partway through Turso WAL writes rolls back bytes and recovers | PASS |
| Small | Checkpoint to immutable R2 object and remove main database pages from DO | PASS |
| Small | Cold start loads R2 base plus newer DO WAL | PASS |
| Small | Restore an independent database using only R2 snapshot and manifest | PASS |
| Small | Failure between R2 upload and manifest publication preserves previous snapshot | PASS |
| Small | Failure within manifest transaction keeps old snapshot and durable pages | PASS |
| Small | Uncommitted transaction disappears after full runtime replacement | PASS |
| Small | 100 concurrent HTTP increments preserve every update in one DO | PASS |
| Small | Independent tenant Durable Objects keep separate databases | PASS |
| Small | Local request latency measurements | PASS |
| Small | MVCC BEGIN CONCURRENT and write conflict detection | PASS |
| Small | MVCC permits non-conflicting transactions on two connections | PASS |
| Small | MVCC logical log recovers after full runtime replacement | PASS |
| Small | MVCC checkpoint and R2-only restore preserve all database sidecars | PASS |
| Small | Checkpoint refuses to snapshot an active transaction | PASS |
| Small | Oversized transaction exceeds the staged write budget without partial durability | PASS |
| Large | 32 MiB: seed actual database into immutable R2 segments | PASS |
| Large | 32 MiB: cold indexed query reads only required R2 ranges | PASS |
| Large | 32 MiB: warm lookup reuses cached pages | PASS |
| Large | 32 MiB: 300 random indexed lookups | PASS |
| Large | 32 MiB: indexed join and range query | PASS |
| Large | 32 MiB: full data scan with bounded cache | PASS |
| Large | 32 MiB: durable update, WAL recovery and incremental R2 checkpoint | PASS |
| Large | 32 MiB: restore updated database using R2 and manifest alone | PASS |
| Large | 256 MiB: seed actual database into immutable R2 segments | PASS |
| Large | 256 MiB: cold indexed query reads only required R2 ranges | PASS |
| Large | 256 MiB: warm lookup reuses cached pages | PASS |
| Large | 256 MiB: 300 random indexed lookups | PASS |
| Large | 256 MiB: indexed join and range query | PASS |
| Large | 256 MiB: full data scan with bounded cache | PASS |
| Large | 256 MiB: durable update, WAL recovery and incremental R2 checkpoint | PASS |
| Large | 256 MiB: restore updated database using R2 and manifest alone | PASS |
| Large | 1024 MiB: seed actual database into immutable R2 segments | PASS |
| Large | 1024 MiB: cold indexed query reads only required R2 ranges | PASS |
| Large | 1024 MiB: warm lookup reuses cached pages | PASS |
| Large | 1024 MiB: 300 random indexed lookups | PASS |
| Large | 1024 MiB: indexed join and range query | PASS |
| Large | 1024 MiB: full data scan with bounded cache | PASS |
| Large | 1024 MiB: durable update, WAL recovery and incremental R2 checkpoint | PASS |
| Large | 1024 MiB: restore updated database using R2 and manifest alone | PASS |
| Large | largest database: 16 MiB cache / 4 KiB R2 blocks | PASS |
| Large | largest database: 16 MiB cache / 64 KiB R2 blocks | PASS |
| Large | largest database: 16 MiB cache / 256 KiB R2 blocks | PASS |
| Large | largest database: 32 MiB cache / 4 KiB R2 blocks | PASS |
| Large | largest database: 32 MiB cache / 64 KiB R2 blocks | PASS |
| Large | largest database: 32 MiB cache / 256 KiB R2 blocks | PASS |
| Large | largest database: full scan with 32 MiB cache | PASS |
| Large | largest database: rebuild the entire R2 snapshot with bounded segment buffers | PASS |
| Large | largest database: 100 concurrent updates survive restart | PASS |
| Large | largest database: error R2 read fails safely and retry recovers | PASS |
| Large | largest database: short R2 read fails safely and retry recovers | PASS |
| Large | largest database: remote failure after staged WAL writes leaves no partial commit | PASS |
| Large | largest database: after-upload snapshot failure retains prior snapshot and committed update | PASS |
| Large | largest database: during-manifest snapshot failure retains prior snapshot and committed update | PASS |
| Large | largest database: injected 5 ms delay per R2 read | PASS |
| Large | largest database: injected 20 ms delay per R2 read | PASS |
| Readers | seed 1 GB database into immutable R2 segments | PASS |
| Readers | publish a versioned owner checkpoint and pin four reader actors | PASS |
| Readers | reader denies SQL writes, chained statements and snapshot publication | PASS |
| Readers | read-only owner queries do not advance the commit version | PASS |
| Readers | owner reader scan(s) single with 30 checkout requests | PASS |
| Readers | 1 reader scan(s) single with 30 checkout requests | PASS |
| Readers | 2 reader scan(s) single with 30 checkout requests | PASS |
| Readers | 4 reader scan(s) single with 30 checkout requests | PASS |
| Readers | 1 reader scan(s) sliced with 30 checkout requests | PASS |
| Readers | 2 reader scan(s) sliced with 30 checkout requests | PASS |
| Readers | 4 reader scan(s) sliced with 30 checkout requests | PASS |
| Readers | stale reader remains on pinned snapshot; version requirement routes to owner | PASS |
| Readers | every checkout created one order and reduced stock once | PASS |
| Owner control | seed 1 GB database into immutable R2 segments | PASS |
| Owner control | publish a versioned owner checkpoint and pin four reader actors | PASS |
| Owner control | reader denies SQL writes, chained statements and snapshot publication | PASS |
| Owner control | read-only owner queries do not advance the commit version | PASS |
| Owner control | owner reader scan(s) sliced with 30 checkout requests | PASS |
| Owner control | stale reader remains on pinned snapshot; version requirement routes to owner | PASS |
| Owner control | every checkout created one order and reduced stock once | PASS |
