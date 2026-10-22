# Contributing to nota

nota is in early development. Issues and pull requests are welcome; for
anything larger than a small fix, open an issue first so the change can be
placed in the plan.

## Issue tracking

Work is tracked in GitHub issues in this repo.

- **The epic.** Every issue is a sub-issue of #144, `epic: nota v1`.
- **Milestones** follow the development plan: `M3: term clean-up` to
  `M7: v1 release`. `Next: not yet planned` holds work that's accepted but
  not scheduled, and `Holding` holds work waiting on a trigger, such as a
  measurement or an upstream release.
- **Order** comes from GitHub's "blocked by" links, not from text.
- **Status and priority** are kept on the maintainer's project board.

Each issue has exactly one type label:

| Label | Use it for |
|---|---|
| `type:epic` | A body of work; its sub-issues deliver it |
| `type:feature` | Changes what nota does for a user |
| `type:research` | Answers a question; the output is a note or a measurement, not code |
| `type:bug` | Incorrect behaviour |
| `type:chore` | No behaviour change: upgrades, refactors, CI, tooling |

Other labels:

| Label | Means |
|---|---|
| `tier:routine`, `tier:standard`, `tier:critical` | How much review the change needs. Critical covers the recorder's write path, the session clock, the engine protocol, the code checks, the storage schema, anything that deletes or rewrites stored audio or transcripts, and CI and check configuration |
| `follow-up` | Work a pull request left for later |
| `decision` | Waiting on a decision before it can be built |
| `needs-triage` | Not yet placed in a milestone |

New issues start from a template (bug, feature or chore), which sets the
type label.

## Pull requests

- Branch from `main` as `<issue number>-<short-slug>`.
- Say `Closes #N` for each issue the pull request finishes, so it closes on
  merge.
- `main` takes squash merges only, after the required checks pass. Run
  `lefthook install` once per clone so the same checks run before you push.
- A change that weakens a check (a new `#[allow]`, `#[expect]` or
  `#[ignore]`, lint or check config, a deleted test, a changed snapshot)
  needs a line starting `Check changes:` in the description that says why.
- Never commit recordings, transcripts or anything taken from a real lecture.
  Tests and screen snapshots use invented text.

By contributing, you agree that your contribution is dual licensed under
MIT OR Apache-2.0, as the README says.
