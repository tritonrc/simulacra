# R011 — Protocol Enforcement

## Rule

The 5-phase protocol (Scope → Red → Green → Refactor → Review) is mandatory for every implementation task. Skipping, reordering, or self-performing a delegated phase invalidates the work.

### Phase Boundaries Are Hard Walls

| Phase | Who | Violation |
|---|---|---|
| 0 — Scope | Claude (main conversation) | — |
| 1 — Red | `codex exec --model gpt-5.5 --full-auto --cd . "..." < /dev/null` | Claude writes tests directly |
| 2 — Green | Claude sub-agent (one per task) | Claude writes implementation in the main conversation |
| 3 — Refactor | `cargo` (mechanical) | Any gate skipped |
| 4 — Review | `codex exec --model gpt-5.5 --cd . "..." < /dev/null` | Claude reviews its own work |
| 5 — Commit | Claude (main conversation) | Committed before all gates pass |

> `codex exec` hangs unless stdin is closed (`< /dev/null`); macOS has no `timeout`.

### What Claude MUST NOT Do

1. **Write tests.** Phase 1 shells to `codex exec --model gpt-5.5 --full-auto --cd . "..." < /dev/null`. If Claude writes a `#[test]`, delete it and redo Phase 1. No exceptions.
2. **Write implementation code in the main conversation.** Phase 2 delegates to a sub-agent. The main conversation orchestrates — it does not implement.
3. **Review its own work.** Phase 4 shells to `codex exec --model gpt-5.5 --cd . "..." < /dev/null`. If Claude performs the review, it is invalid. Redo Phase 4.

### Mechanical Gates Are Non-Negotiable

All four must pass **before** Phase 4 review begins:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Do not proceed to review if any gate fails. Fix the failure first.

### Phase Order Is Sequential

Phases execute in order: 0 → 1 → 2 → 3 → 4 → 5. No skipping. No reordering.

- **No green without red.** Implementation without failing tests is untestable.
- **No review without refactor.** Mechanical issues waste the reviewer's attention.
- **No commit without review.** Unreviewed code does not enter the repository.

### Detecting Violations

If you notice any of these, stop and correct:

| Symptom | Violation | Fix |
|---|---|---|
| Claude writing `#[test]` functions | Phase 1 bypass | Delete tests, shell to Codex (gpt-5.5) |
| Implementation code in main conversation | Phase 2 bypass | Discard, delegate to sub-agent |
| `cargo clippy` or `cargo fmt` not run | Phase 3 incomplete | Run all 4 mechanical checks |
| Claude summarizing code quality instead of shelling to Codex | Phase 4 bypass | Shell to Codex (gpt-5.5) |
| Commit before review | Phase 4 skipped | Reset commit, run review |

### Restarting After a Violation

If any phase was skipped or self-performed:

1. The work product of that phase is invalid.
2. Redo the violated phase correctly.
3. Re-run all subsequent phases (violations can cascade).

## Why

The protocol exists to enforce adversarial diversity: Codex (gpt-5.5) writes tests and reviews, Claude implements, and the human remains the architectural authority. When Claude writes its own tests, it tests what it plans to implement, not what the spec requires. When Claude reviews its own code, it has the same blind spots as the author. The mechanical gates catch what all models miss. Every shortcut reduces the probability that bugs are caught before commit.
