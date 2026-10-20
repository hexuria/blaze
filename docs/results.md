# Results

Every number here names the harness that printed it, the commit it was measured at, and
the report it was copied from. Nothing is averaged or rounded beyond what the source
printed. Most numbers are from a single run on one machine.

How to read the commit column. The work was done on separate branches that were merged
into one, so the commits differ from section to section. Where a report names the commit
it measured, that commit is given. Where it only says "the head of this branch", the commit
given is the one that last changed the report, marked "report at". Commits are of the
development branch (`rustd-collate` and the branches merged into it), not of the published
tree; `vendor/source.rev` says which development commit a published tree was made from.

Reports are named by file. Those under [reports/](reports/) are published with this
tree. BREAKERS.md and the other `harness/*.md` files are in `harness/`. WINDOWS.md,
MACOS.md, MACOS-ABI2.md, AARCH64.md and RUSTD.md are earlier reports that
[platforms.md](platforms.md) and these documents replace; they are on the development
branch only.

Where two reports give different numbers for the same thing, the later report is used and
the difference is listed under [Disagreements](#disagreements).

To update this file: rerun the harness named in the row, replace the row, and change the
commit. Do not append a second row for the same measurement.

## 1. Checked for these documents

Run on Linux x86-64 (8 CPUs, 31 GB) with Rust 1.99.0 at f191c6e42a, the commit these
documents were written against. rustd was built with `cargo build -p rustd --release`, and
for the LLVM column with `--features llvm` against LLVM 20.1.8.
Source trees and scratch directories were on tmpfs.

The gate, from `harness/` with `RUSTD_BIN` set and `TMPDIR` on tmpfs:

<!-- LOCAL-GATE: replace this table when the gate is rerun. -->

| Harness | Cranelift | LLVM (`RUSTD_OPT=2 RUSTD_EMIT=fast`) |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 822 of 822 functions from rustd, 0 of 893 layouts differ | MATCH, 822 of 822, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 58 of 58, 7 open, 0 functions skipped | 58 of 58, 7 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 62 of 62 | 62 of 62 |
| `keys.py` | 46 of 46, 0 open | 46 of 46, 0 open |
| `fuzz/fuzz.py smoke` | 2 programs, 505 functions compiled, 0 differences, 0 crashes, 0 left to rustc | 2 programs, 505 functions compiled, 0 differences, 0 crashes, 0 left to rustc |
| `debuginfo.py` | all right | all right |
| `backends.py corpus runner cases --bin runner --levels 2f` | does not apply | 11,801 of 11,801 functions compiled by LLVM, none left to Cranelift; the program prints the same as cargo's |

The seven "open" breakers are the five known differences and the two known refusals
listed in [support.md](support.md). Not run here: `buildmodel.py`, `farsplices.py`, the
four store harnesses, `remote.py` and `cargo_rustd.py`.

By hand, with `cargo rustd` from that build:

| What | Result |
|---|---|
| New `cargo new` project: `cargo rustd build`, `run`, edit, `run` | first build cargo's; edit served by the daemon; `Finished` in 0.01 s |
| ripgrep 15.1.0: `cargo rustd build` | cargo's, 5.6 s; daemon ready within 10 s |
| ripgrep: one-line edit to `crates/core/main.rs`, `cargo rustd build` | by the daemon, 0.26 s wall; the binary prints the edit |
| ripgrep: `cargo rustd test` | by the daemon; 114 and 329 tests pass, the same as cargo's |
| ripgrep: second git worktree | first build cargo's, 16 s; joined the same worker; edit by the daemon, 0.44 s wall |
| `RUSTD_CACHE` set when the daemon starts | store filled: 1 pack, 26,377 records, 57,153,509 bytes; `rustd cache verify` exit 0 |
| `rustd cache push` and `pull` to a directory remote | 1 pack, 7,744,971 bytes compressed, sent and fetched |
| Published layout: recursive clone, `cargo build -p rustd --release --locked` | builds |

## 2. Corpus and layouts, per platform

`difftest.py corpus --bin runner --crate cases --layouts`. "Functions" is how many
functions of the corpus rustc emitted a symbol for and rustd compiled; the rest would be
left to rustc. "Layouts" is how many types rustd laid out differently from rustc's
`-Zprint-type-sizes`. The LLVM columns are from
`backends.py corpus runner cases --bin runner --levels 2f` and count instances.

| Platform | Functions from rustd, both backends | Layouts that differ | LLVM compiled | Left to Cranelift | Commit | Source |
|---|---|---|---|---|---|---|
| Linux x86-64 | 822 of 822 | 0 of 893 | 11,787 of 11,787 | 0 | 888e87df74, CI run 39057174862 | PLAT-ABI2.md |
| Linux AArch64, native | 825 of 825 | 0 of 903 | 11,813 of 11,813 | 0 | same | PLAT-ABI2.md |
| Windows x86-64 MSVC | 830 of 830 | 0 of 898 | 11,801 of 11,801 | 0 | same | PLAT-ABI2.md |
| macOS arm64 | 828 of 828 | 0 of 914 | 11,879 of 11,879 | 0 | same | PLAT-ABI2.md |
| wasm32-wasip1 (wasmtime) | 655 of 656 | not run | no LLVM | | report at 0d0588c9f1, nightly-2026-05-10 | WASM32.md |

The rest of the gate on that CI run, on every leg: programs, stress, breakers (43 of 43, 5
open), unwinds, splices, asmedits, includes, worktrees, keys (36 of 36, 0 open),
storekills, storecorrupts, storeshared. The Linux legs add `debuginfo.py`, `storefull.py`
and `buildmodel.py`; AArch64 adds `farsplices.py`. The report does not print the counts of
each harness per leg, only that the legs passed.

The wasm row is from an earlier branch on a nightly toolchain, when the corpus was
smaller. It has not been rerun on 1.99.0 and is not in CI.

## 3. The gate on Linux x86-64

Counts printed by each harness, Cranelift, on a254756b77 (PLAT-ABI2.md, "Local"). LLVM was
not rerun locally on that commit; the CI legs ran it.

| Harness | Result |
|---|---|
| `difftest.py corpus --layouts` | MATCH, 822 of 822, 0 of 893 layouts |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99 |
| `breakers.py` | 43 of 43, 5 open |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `farsplices.py` | 6 of 6 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 58 of 58 |
| `keys.py` | 36 of 36 |
| `debuginfo.py` | all right |
| `buildmodel.py` | 208 of 208 (CI on `51339e5`) |

`buildmodel.py` on native AArch64: 204 of 204, with four rows that need an x86-64 host not
run (the same CI run).

## 4. rustc's own test suites

Linux x86-64, Rust 1.99.0, the rust-lang/rust checkout at the `1.99.0` tag. Single runs.
Report at b46bc7b8ec, branch `rustd-rustc-suites` off 6d12f21c86. Source: RUSTCSUITES.md.
Harnesses: `uitest.py` and `libsuite.py`.

| Suite | Backend | Tests run | Matching | Differing | Left out | Functions from rustd | Left to rustc |
|---|---|---|---|---|---|---|---|
| `tests/ui` run-pass | Cranelift | 2690 | 2686 | 4 | 1114 | 5558 of 5615 | 57 |
| `tests/ui` run-pass | LLVM | 2689 | 2685 | 4 | 1115 | 5554 of 5611 | 57 |
| `coretests` | Cranelift | 1197 | 1197 | 0 | | 1491 of 1499 | 8 |
| `coretests` | LLVM | 1197 | 1197 | 0 | | 1491 of 1499 | 8 |
| `alloctests` | Cranelift | 703 | 703 | 0 | | 964 of 966 | 2 |
| `alloctests` | LLVM | 703 | 703 | 0 | | 964 of 966 | 2 |
| std's tests (28 binaries) | Cranelift | 252 | 252 | 0 | | 380 of 381 | 1 |
| std's tests (28 binaries) | LLVM | 252 | 252 | 0 | | 380 of 381 | 1 |

What these do not cover. Of 3804 run-pass tests (counting revisions), 1114 are left out:
703 need a nightly feature, 76 a `-Z` flag, 73 have no top-level `main`, 48 have an
auxiliary crate that needs nightly, 47 set an optimisation level, and the rest are smaller
groups listed in the report. The library suites are cut down to what builds on stable:
937 of 1443 `#[test]` functions of `coretests`, 643 of 951 of `alloctests`, 259 of 421 of
std's. The unit tests inside `core`, `alloc` and `std` are not run at all. Only Linux
x86-64 was run.

The four differing tests are three causes, all listed as open in
[support.md](support.md): the size of a future (two tests), a value rustc leaks and rustd
drops, and a macro fragment read by a different edition.

## 5. The crate sweep (partial)

This is a sweep of 93 crates chosen by hand, not of crates.io. Seven of the 93 gave no
result (tests that do not build from the published crate, a missing system library, a
timeout). Each crate's own integration tests are built by cargo and again with rustd's
objects linked in front, and every test must report the same.

Harness: `cratesweep.sh` and `sweepreport.py`. Source: harness/BREAKERS.md, "On stable
1.99". Linux x86-64, Cranelift, Rust 1.99.0, one run, at f296858ee3 plus three fixes.

| What | Result |
|---|---|
| Crates with a result | 86 of 93 |
| Crates whose tests behave as rustc's | 86 of 86 |
| Tests run and compared | 10,745 |
| Functions compiled by rustd, as the sweep ran | 41,958 of 41,987 (29 left to rustc) |
| After the fixes, rerun on the four crates they touch | 15 left, all in generic-array |
| tokio with `--features full` (`cratetest.py`) | 155 test binaries, 1018 tests, all matching, 296,088 of 296,088 functions |

The sweep has not been run under LLVM, or on any platform but Linux x86-64.

## 6. Real applications

### Command lines compared (`bintest.py`)

Each program is built by cargo and with rustd's code for its workspace crates, and a list
of command lines is run with both. Source: harness/BREAKERS.md. Linux x86-64, Cranelift,
`nightly-2026-05-10`, at 1c293327c1. Not rerun on 1.99.0.

| Program | Command lines the same | Functions from rustd |
|---|---|---|
| fd | 10 of 10 | 148 of 148 |
| just | 12 of 12 | 822 of 822 |
| hyperfine | 7 of 7 | 161 of 161 |
| zoxide | 9 of 9 | 131 of 131 |
| ripgrep | 12 of 12 | 1820 of 1820 |
| bat | 10 of 10 | 321 of 321 |
| tokei | 8 of 8 | 141 of 141 |

### `cargo rustd` against cargo (`cargo_rustd.py`)

Linux x86-64, 8 CPUs, 31 GB, Rust 1.99.0, Cranelift. Report at 13054e50d6 (the head of
the cargo branch with `rustd-collate` b43304222b merged). Source: harness/CARGO-RUSTD.md.
Seconds, `cargo rustd` / plain cargo, one run each. `(c)` marks a step cargo built in the
daemon's place. "Differences" counts steps whose output, exit status or test results were
not cargo's.

| Project | Cold build | Warm-up | Warm build | Cold test | Test warm-up | Warm test | Edit build | Edit test | Broken | Mended | Differences |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| ripgrep 15.1.0 | 6.00 / 6.48 | 6.87 | 0.02 / 0.44 | 2.89 / 6.13 | 3.87 | 0.95 / 1.35 | 0.34 / 0.86 | 2.25 (c) / 1.60 | 0.38 (c) / 0.20 | 0.27 / 0.55 | 0 |
| fd 10.3.0 | 21.91 / 21.98 | 0.02 | 0.03 / 0.55 | 5.90 / 5.52 | 3.02 | 0.64 / 1.05 | 0.16 / 0.51 | 0.90 / 1.03 | 0.57 (c) / 0.23 | 0.17 / 0.41 | 0 |
| bat 0.26.0 | 17.47 / 20.90 | 8.50 | 0.02 / 0.08 | 10.68 / 15.38 | 8.51 | 1.68 (c) / 1.46 | 0.30 / 0.53 | 1.95 / 1.33 | 0.38 (c) / 0.26 | 0.09 / 0.50 | 0 |
| hyperfine 1.19.0 | 5.81 / 6.79 | 5.28 | 0.03 / 0.39 | 4.73 / 4.92 | 2.77 | 0.38 / 0.82 | 0.13 / 0.34 | 0.60 / 0.75 | 0.37 (c) / 0.20 | 0.19 / 0.34 | 0 |
| just 1.43.0 | 8.89 / 10.12 | 2.50 | 0.02 / 0.06 | 8.65 / 7.62 | 8.49 | 1.54 / 1.67 | 0.11 / 0.28 | 1.34 / 1.09 | 0.22 (c) / 0.12 | 0.09 / 0.27 | 0 |
| axum 0.8.6 | 9.06 / 9.20 | 3.86 | 0.02 / 0.13 | 11.44 / 10.70 | 2.23 | 0.03 / 0.13 | 0.07 / 0.40 | 0.12 / 0.97 | 0.33 (c) / 0.19 | 0.06 / 0.44 | 0 |
| nushell 0.108.0 | 44.12 / 54.73 | 42.36 | 0.04 / 0.26 | 3.25 / 2.37 | 1.12 | 0.69 / 0.72 | 0.96 / 1.65 | 0.55 / 0.50 | 1.13 (c) / 0.65 | 1.12 / 1.63 | 0 |

How to read it. The cold columns are cargo on both sides; their differences are noise.
"Warm-up" is the time after the cold build until the daemon has loaded the workspace,
during which builds are cargo's. "Broken" is a type error: always cargo's, and slower than
plain cargo by the time it takes to ask the daemon first. Some "edit test" times are
slower than cargo's.

The same run: 66 of 66 flag steps the same as cargo on a generated workspace, and 11 of 11
daemon lifecycle checks.

Not in the table: in ten runs of just, two had one difference each that is not explained
(see [cargo.md](cargo.md), "Known problems").

### Agent-style edit runs (`agents.py`)

Random edits of the kinds coding agents make, against one warm daemon, each step compared
with cargo's build of the same source. Linux x86-64, Cranelift. The files are generated by
the harness and do not record a commit; they were committed at 47dfd2fbb4 and, for
dioxus-cli, 8411413a56. Times include the link.

| Project | Steps right | Restarts | Median step | Daemon RSS at the end | Warm-up | Source |
|---|---|---|---:|---:|---:|---|
| sapp | 60 of 60 | 0 | 84 ms | 694 MB | 2.4 s | harness/AGENTS-sapp.md |
| ripgrep | 80 of 80 | 0 | 327 ms | 1311 MB | 11.1 s | harness/AGENTS-ripgrep.md |
| hyperfine | 80 of 80 | 0 | 214 ms | 2069 MB | 18.9 s | harness/AGENTS-hyperfine.md |
| just | 80 of 80 | 0 | 484 ms | 2083 MB | 16.6 s | harness/AGENTS-just.md |
| dioxus-cli | 60 of 60 | 0 | 3945 ms | 9370 MB | 183.8 s | harness/AGENTS-dioxus-cli-s2.md |

dioxus-cli is the large case: a step takes about four seconds and the daemon holds 9.4 GB.

## 7. The fuzzer

`fuzz/fuzz.py run`, 30 cases per program, both backends. Linux x86-64, Rust 1.99.0,
LLVM 20.1.8. Source: FUZZ.md, report at 8f21942505.

Twenty-one distinct problems were found, most of them miscompiles. Eighteen are fixed, two
(believed to be LLVM bugs) are worked around, and one is open. The table stops at L10; the
later runs, which found findings 17 to 21, are in FUZZ.md.

| Run | rustd at | Programs | Functions generated | Compiled by rustd, each backend | Differences | Left to rustc |
|---|---|---:|---:|---:|---|---|
| L1 | early fixes | 136 | 39,720 | 41,089 Cranelift, 38,984 LLVM | Cranelift 4, LLVM 7 | Cranelift 12 |
| L3 | fixes 1 to 8 | 360 | 105,869 | 102,426 | 5 per backend | 0 |
| L4 | e04c495c9c | 119 | 35,679 | 33,854 | 0 | 0 |
| L5 | a1f0466b5d | 150 | 44,031 | 43,069 | 0 | 0 |
| L6 | a1f0466b5d | 209 | 62,034 | 59,182 | 0 | 0 |
| L7 | 876f964f06 | 360 | 106,202 | 102,898 | 8 per backend | 0 |
| L8 | 49c4fddd1f | 360 | 105,649 | 102,081 | 1 per backend | 0 |
| L9 | 27a94f2770 | 360 | 106,116 | 101,848 | 0 | 0 |
| L10 | 27920a4f69 (after the merge) | 60 | 17,297 | 16,734 | 0 | 0 |

L2 is left out because L3 repeats its seed. No run crashed rustd. Up to L8, about one
program in a few hundred still found something; L9 is the first full-size run that found
nothing. The fuzzer builds whole programs only and has not been run on another platform.

## 8. The disk store

Linux x86-64. Source: DISKCACHE.md, report at 5ca7a332ea.

| Harness | Backend | Result |
|---|---|---|
| `storekills.py` | Cranelift | 400 + 100 + 100 kills, 0 failures |
| `storekills.py` | LLVM | 200 kills, 0 failures |
| `storecorrupts.py` | Cranelift | 10 rounds of damage, 4 old formats discarded, 1 newer refused: pass |
| `storecorrupts.py` | LLVM | 4 rounds and the same formats: pass |
| `storefull.py` | both | 6 amounts of free space, 12 builds, all right |
| `storeshared.py --rounds 12` | Cranelift | 72 builds right; 360 collections, 117 packs evicted, 8734 verifications found nothing |
| `storeshared.py --rounds 12` | LLVM | 72 builds right; 284 collections, 109 packs evicted, 1863 verifications |
| `keys.py` | both | 36 of 36, 0 open (PLAT-ABI2.md, 888e87df74) |

In 800 kills a torn record was left once and a temporary file once; recovery from both is
also covered by unit tests.

Debug info cost, from DEBUGINFO.md (report at efeea650a2), corpus, best of three, compile
and link: Cranelift 5.47 s without and 6.67 s with debug info; LLVM 6.55 s and 7.08 s.

## 9. The remote

Linux x86-64, 8 cores, Cranelift, MinIO over loopback behind a counting proxy. Each
"machine" is its own home directory, checkout, target directory and store on the same
host. One run each. Real AWS was not run. Source: REMOTE.md, report at 90017bf2d5.
Harness: `remote.py --measure`.

ripgrep 14.1.1, 9 crates by rustd, 22,340 functions compiled cold:

| Fresh machine | To a running program | cargo | rustd | Functions compiled | Requests | Bytes down |
|---|---:|---:|---:|---:|---:|---:|
| no remote | 16.8 s | 4.6 s | 12.2 s | 22,340 | 0 | 0 |
| rustd's packs | 14.3 s | 4.8 s | 9.5 s | 0 | 3 | 7,355,315 |
| packs and cargo's units | 9.6 s | 2.0 s | 7.3 s | 0 | 45 | 59,161,263 |
| the same, lazy | 10.0 s | 2.1 s | 7.6 s | 0 | 46 | 59,946,863 |

cargo 0.92.0, 7 crates by rustd, 226,180 functions compiled cold:

| Fresh machine | To a running program | cargo | rustd | Functions compiled | Requests | Bytes down |
|---|---:|---:|---:|---:|---:|---:|
| no remote | 152.4 s | 42.0 s | 110.3 s | 226,180 | 0 | 0 |
| rustd's packs | 117.1 s | 43.4 s | 73.6 s | 0 | 18 | 95,048,041 |
| packs and cargo's units | 94.4 s | 25.2 s | 67.4 s | 0 | 406 | 525,028,848 |
| the same, lazy | 100.2 s | 25.5 s | 73.1 s | 0 | 422 | 532,207,920 |

With every function served from the remote, rustd still takes 7.3 s on ripgrep and 67 s on
cargo. That is loading the workspace and walking every instance to find its key.

The robustness part of `remote.py` (broken transfers, spoiled objects, forged manifests,
collection) passes on MinIO and on SeaweedFS; collection is 12 of 12 on both.

## 10. Edit and warm-up timings

The warm-up and edit times of `cargo rustd` are in section 6. The numbers below are of the
daemon driven directly, which leaves out the client.

### Full link against splice

| Platform | Program | Full link | Splice | What is timed | Source |
|---|---|---:|---:|---|---|
| Windows x86-64 | toy, 7 functions | 98 ms | 9.4 ms | daemon's edit to binary, median of 10 | WINDOWS.md |
| Windows x86-64 | regex-automata | 548 ms | 18 ms | same | WINDOWS.md |
| macOS arm64 | small program | 50 ms | 23 ms | whole request, median of 100 | MACOS.md |
| macOS arm64 | ripgrep 14.1.1 | 104.8 ms | 58.5 ms | daemon's edit to binary, median of 50 | MACOS.md |
| Linux x86-64, AArch64 target | stress app | 131.3 ms | 6.1 ms | `rustd sync`, median of 10 | AARCH64.md |
| Linux x86-64, AArch64 target | ripgrep 14.1.1 | 2018.5 ms | 18.9 ms | same; the full link is GNU ld | AARCH64.md |
| Linux x86-64, wasm32-wasip1 | small std program | 57 ms | 11 ms | edit to a program that can be run | WASM32.md |
| Linux x86-64, wasm32-unknown-unknown | Dioxus hello | 193 ms | 35 ms | same, before 0.27 to 0.36 s of wasm-bindgen | WASM32.md |

These come from the first port to each platform, measured on `nightly-2026-05-10` on
different machines. They are not comparable across rows. There is no Linux x86-64 native
row in the reports for the same measurement.

### What an edit costs the database (`invalidation.py`)

Linux x86-64, the vendored regex-syntax and regex-automata (`harness/bench2`), 17,260
functions and instances. Source: harness/INVALIDATION-regex_syntax+regex_automata+bench2.md,
committed at a1d1676eba. Over its 30 kinds of edit, edit to binary was 498 to 1200 ms
against 250 to 3276 ms for `cargo build`, with 0 to 60 functions recompiled and the
program's output the same as cargo's in every row.

### Generated early and stale

`harness/SCOREBOARD.md` and `harness/BENCH.md` were generated on 12 and 13 October at
58b42c3491 and 923fc86e76, before most of the work above, when the corpus had 152
functions. Their numbers are not repeated here. They need regenerating before they are
quoted.

## Disagreements

Where reports disagree, this file follows the later one.

| Subject | Earlier | Later, used here |
|---|---|---|
| Corpus size on Linux x86-64 | 661 of 662 (BREAKERS.md), 679 of 680 (WASM32.md), 803 of 803 (BUILDMODEL.md, CARGO.md, DISKCACHE.md), 808 of 808 (AARCH64.md) | 822 of 822 (PLAT-ABI2.md). The corpus grew, and the one function left to rustc was fixed |
| Corpus on Windows | 827 of 827 (WINDOWS.md) | 830 of 830 (PLAT-ABI2.md) |
| Corpus on macOS | 816 of 816 (MACOS-ABI2.md), 826 of 828 (PLAT-ABI2.md, before a254756b77) | 828 of 828 |
| Corpus on AArch64 | 817 of 817 (AARCH64.md) | 825 of 825 |
| `breakers.py` | 25 of 25, 0 open (BREAKERS.md, BUILDMODEL.md, CARGO.md, DEBUGINFO.md); 43 of 43, 5 open (RUSTCSUITES.md, PLAT-ABI2.md) | 58 of 58, 7 open (section 1). Cases were added from rustc's suites and the fuzzer |
| `keys.py` | 26 of 26, 4 open (AARCH64.md, WINDOWS.md, MACOS-ABI2.md, DISKCACHE.md); 36 of 36, 0 open (CARGO.md, PLAT-ABI2.md) | 46 of 46, 0 open (BUILDMODEL.md, and section 1). The count depends on which branches were merged; none is open at the head |
| `worktrees.py` | 52 of 52 (DEBUGINFO.md), 58 of 58 (PLAT-ABI2.md) | 62 of 62 (BUILDMODEL.md, and section 1) |
| `buildmodel.py` | 97 of 97 per backend (BUILDMODEL.md); 179 of 180 with `cross_aarch64` under LLVM not finishing (CARGO.md) | 180 of 180 (PLAT-ABI2.md) |
| Store platforms | "Linux x86-64 only" (DISKCACHE.md) | store harnesses pass in CI on Windows and macOS too (PLAT-ABI2.md) |
| S3 | "The S3 remote path was not exercised" (DISKCACHE.md) | exercised against MinIO and SeaweedFS (REMOTE.md); real AWS still not run |
| `sym` in `naked_asm!` on Mach-O and COFF | "not read back" (AARCH64.md, WINDOWS.md) | compiled on all four platforms (PLAT-ABI2.md) |
| Aggregates aligned to 16 bytes on the x86-64 stack | open bug (MACOS-ABI2.md) | fixed (PLAT-ABI2.md) |
| Toolchain | `nightly-2026-05-10` (BREAKERS.md first passes, WINDOWS.md, MACOS.md, AARCH64.md first rounds, WASM.md, WASM32.md) | stable 1.99.0 everywhere else |
| Goals | "Hot patching" and optimised code are "not goals"; "What to build next" lists the panic fixtures, the layout check, random programs and CI (RUSTD.md) | all four exist; the splice exists; RUSTD.md is the original plan and is out of date |
| `cargo rustd -q` | listed among the flags handed to cargo (CARGO.md) | not in the list in `cargo/cli.rs`. Checked by hand: the first `-q` build is cargo's because `-q` makes it a different command line, and later ones are the daemon's |
