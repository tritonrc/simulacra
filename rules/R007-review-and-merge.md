# R007 — Review and Merge (Multi-Model)

## Rule

Code review is performed by a **different model** than the author. This is a blocking gate, not optional.

### Who Does What

| Step | Model | Tool | Responsibility |
|---|---|---|---|
| Mechanical checks | CI / local | `cargo` | Build, test, clippy, fmt |
| Code review | Gemini 3 Pro | `copilot --model gemini-3-pro-preview` | Spec compliance, architectural review |
| Blocker resolution | Claude Code | sub-agent | Fix BLOCKERs found by reviewer |

**Claude MUST NOT review its own work.** Review is done by shelling out to copilot with Gemini. If Claude performs the review, it is invalid — redo via copilot.

### Step 1: Mechanical Checks (Non-Negotiable Gate)

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four must pass before review. Do not proceed to Step 2 if any fail.

### Step 2: LLM Review — Exact Command

```bash
copilot --model gemini-3-pro-preview --allow-all --prompt "You are a senior Rust \
  reviewer for the Simulacra project. Review the CHANGES (not existing code) in \
  crates/simulacra-<crate>/. \
  Read specs/S00N-*.md, ARCHITECTURE.md, and relevant rules/ files. \
  Classify each issue as BLOCKER (must fix), WARNING (should fix), or NIT (optional). \
  Focus on: \
  1. Spec compliance — does the code satisfy every assertion? \
  2. Journal completeness — every side effect journaled before return? \
  3. Capability enforcement — checked at the call site? \
  4. No invented behavior — if spec doesn't cover it, it shouldn't exist. \
  5. Test coverage — every spec assertion has a test? \
  6. Error handling — no .unwrap() in library code (R003). \
  7. Dependency rule — deps flow downward, no new unjustified deps."
```

### Step 3: Resolve Findings

- **BLOCKER:** Must fix. Then re-run Step 1 + Step 2.
- **WARNING:** Fix or justify with a comment in the commit message.
- **NIT:** Fix if trivial, skip if not.

### Merge Criteria

- All 4 mechanical checks green
- LLM review: zero BLOCKERs
- Commit message references the spec: `feat(<crate>): <what> [S00N]`

### What the Reviewer Checks

1. **Spec compliance.** Does the code do what the spec says? Not more, not less.
2. **Scope.** Does the change stay within its stated scope?
3. **Journal completeness.** Every side effect journaled before return?
4. **Capability enforcement.** Proxy layer checks capabilities?
5. **Test coverage.** Every spec assertion has a test?
6. **Dependency rule.** Any new deps justified?
7. **No invented behavior.** If the spec doesn't cover it, it shouldn't be in the code.

## Why

Three-model workflow (GPT-5.4 tests, Claude implements, Gemini reviews) creates adversarial diversity. Each model has different training biases, different blind spots, different strengths. The intersection of their agreement is more likely to be correct than any single model's output. The human remains the architectural authority — models do the volume work.
