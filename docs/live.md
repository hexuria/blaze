# Live edits

A program that links `ra-jit-live` takes each edit while it runs. This is what happens between
the save and the new code running, and what is refused.

## The two halves

- **The daemon** (`crates/rustd/src/live.rs`, `crates/rustd/src/splice/macho.rs`). Every edit
  it answers with a patch (the functions that changed, linked against the executable of the
  last full link) is spliced into the executable on disk as rustd always did. If a program
  that runs that executable has said where it wants patches, the daemon also lays the same
  patch out for that place (`Patch::place_live`) and writes it next to the program.
- **The program** (`crates/ra-jit-live`). `Live::start` reads the executable's `LC_UUID`,
  sets aside 512 MB right behind the image (within `adrp`'s 4 GB, so new code reaches all of
  the old), says so in a file, and watches for patches on a thread. `Live::poll`, called
  between frames, applies the newest one.

They meet in `~/.cache/ra-jit/live/<uuid>/`: `process` (written by the program: pid, uuid,
when it started, the next free address and the end of its room) and `patch` (written by the
daemon, `RAJITLV1` followed by the encoding in `crates/ra-jit-live/src/format.rs`). The
directory is keyed by the UUID because the daemon's path for the executable and the program's
(cargo's hard link in `target/debug`) differ.

## Applying a patch

1. Map the text at its base, slide its pointers by the program's ASLR slide, make it
   read-and-execute and invalidate the instruction cache. Map the data the same way.
2. Register its unwind information (`__register_frame`, one FDE at a time), so a panic in new
   code unwinds through it.
3. Write the jumps over the old entries and the statics' new values. A page that is writable
   is written in place; any other gets a copy that is written, set to the original protection
   and swapped in with `mach_vm_remap(FIXED | OVERWRITE)`, since macOS refuses a write to text.
4. Put back what an earlier patch wrote and this one no longer does: the daemon sends the
   linker's original bytes for every place a splice ever wrote, because a program started from
   a spliced file has the splice's bytes, not the linker's.

Each patch is laid out behind the last: code of an earlier one may still be on some thread's
stack.

## Calls into the old executable

In the splice and in a live patch, a call or an address taken of a function the executable
already has goes to its original entry, never to a copy a previous splice appended. The entry
is where the jump is written, so a later edit of that function reaches every caller.

## What is refused

The program is told to start again (`Outcome::Restart`) when:

- an edit changes a type the rest of the program sees: the daemon hashes each file's type
  definitions (structs, enums, unions, aliases, traits without default bodies, and `repr`)
  before and after, and a program started before the change can't have its live data
  relaid;
- the edit wrote the whole object again, which the daemon does when it has no compiled base
  to patch against (the first edit after it starts);
- the executable was linked again: the daemon says so for a program it knew, and the program
  watches its own executable for another `LC_UUID`, which covers a daemon started since and a
  plain `cargo build`.

A patch that can't be applied (`Outcome::Declined`) leaves the program as it was.

## Signing

On macOS the linker signs ad hoc. A package with

```toml
[package.metadata.ra-jit]
entitlements = "jit.entitlements"
```

gets `codesign --force --sign - --entitlements <file>` run by the daemon's link command on its
own copy of the executable, which cargo then copies into `target/debug`. A splice reads
`codesign`'s superblob (the code directory, requirements, entitlements, DER entitlements and
the empty CMS blob), keeps every blob but the directory, and rewrites the directory's page
hashes with its page size (4 KB from the linker, 16 KB from `codesign`). An executable signed
again between two splices is read again, and the hashes of the pages the last splice put back
are recomputed from what it put back.
