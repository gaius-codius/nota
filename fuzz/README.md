# Fuzzing

`fuzz/` is a separate Cargo workspace (it is excluded from the root one), so
`cargo nextest` doesn't run it. It is for occasional long runs with AFL++
through `cargo-afl`. CI builds the targets with `cargo afl build --locked`,
so they keep compiling, and runs clippy on them, but doesn't fuzz. Its
`Cargo.toml` copies the root workspace's lint table, which a separate
workspace can't inherit; keep the two the same. The pre-push hook runs the
same clippy. CI sets
`RUSTFLAGS="-C target-cpu=x86-64"`, which cargo-afl appends after its own
`-C target-cpu=native`, and rustc uses the last one: the job's cached
`target/` can come from a runner with a different CPU, and build scripts and
proc macros built for a newer CPU crash with SIGILL on an older one. When a
change to the recorder's dependencies makes that fail on `fuzz/Cargo.lock`, run `cargo metadata` in
`fuzz/` (it adds the missing packages and keeps the rest) and commit the
lock file.

## journal_reader

Feeds arbitrary bytes to `nota_recorder::journal::read_journal` and asserts
the reader's contract on the result:

- `valid_len()` is at most the input length
- no header means no frames and `valid_len() == 0`
- a returned header matches the input's bytes, checked independently of the
  reader: magic, version 3 (or 2, the older layout with no anchor), a
  matching CRC, and the id, track, epoch, rate and anchor where its layout
  puts them; it re-encodes to those bytes
- frame sequence numbers are 0, 1, 2, ... in order
- each frame has between 1 and `MAX_FRAME_SAMPLES` samples, and as many as
  its range says
- every frame is the header's track (one track per journal), and each
  frame starts where the previous one ended
- no frame starts before the epoch's first sample a version 3 header gives
- `valid_len()` equals the header plus the size of every frame
- each returned frame's bytes in the input carry a matching CRC, checked
  independently of the reader, and decode to the samples returned
- `ReadEnd::Complete` means `valid_len()` equals the input length
- a frame refused as another track's stops the read at `valid_len()`, and
  really does carry a track other than the header's
- re-reading `data[..valid_len()]` gives the same header, the same frames and
  `Complete`

A violated assertion panics, which AFL saves as a crash.

## Running it (Linux, no root)

```sh
cargo install cargo-afl
cd fuzz
cargo run --bin make_seeds        # writes synthetic seed journals to in/
cargo afl build --release
AFL_SKIP_CPUFREQ=1 AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1 \
  cargo afl fuzz -i in -o out target/release/journal_reader
```

The first `cargo afl build` builds the AFL++ runtime if it is missing
(`cargo afl config --build`). A plain `cargo build` fails to link
`journal_reader`, because the AFL runtime is only linked by `cargo afl build`.
`make_seeds` builds with plain cargo.

`in/`, `out/` and `target/` are git-ignored.

## When it finds a crash

Turn the crashing input into a unit test in `crates/nota-recorder/src/journal/`
that passes the bytes to `read_journal`, then fix the reader. Seeds are
synthetic. Never commit an input that contains recorded audio; write the
minimal bytes into the test by hand instead.

## engine_protocol

Reads arbitrary bytes as a stream of engine-protocol frames, in each
direction, with `nota_core::protocol::FrameReader`, and asserts on every
frame it accepts:

- it re-encodes to exactly the bytes it was read from, so no two byte
  strings mean the same message
- a transcript covers some samples, and each of its words has text, lies
  inside the transcript's range, and starts no earlier than the word before
  ends

Seeds are any encoded frames. A handshake is enough to start from:

```sh
mkdir -p in-protocol
printf '\x07\x00\x00\x00\x00nota\x01\x00' > in-protocol/hello
cargo afl build --release
AFL_SKIP_CPUFREQ=1 AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1 \
  cargo afl fuzz -i in-protocol -o out-protocol target/release/engine_protocol
```

`in-protocol/` and `out-protocol/` are git-ignored too.

The property tests in
`crates/nota-core/src/protocol/props.rs` cover the same contract on every
`cargo nextest` run.
