# Follow-ups

Date: 2026-10-15

Open lint and tooling items. One bullet each: what, where, why.

## Lints

- **`clippy::map_err_ignore` trial: 64 hits** (store 29, recorder 26, the rest elsewhere). They throw away the error source in `map_err(|_| ...)`. Worth cleaning up so errors keep their cause, then turning the lint on. `missing_docs_in_private_items` (810 hits) and `let_underscore_must_use` (209) were also trialled and skipped: too noisy for the value.
- **About 40 `#[expect(clippy::disallowed_methods)]` in test modules.** They could become one allow per test module, which means fewer lines for the weakened-checks detector to flag and less noise in diffs.

## Mutation testing

- **One baseline run failed once, then passed.** An unidentified flaky test. Worth catching when it fails again (keep the log) because a flaky baseline makes mutant results unreliable.
