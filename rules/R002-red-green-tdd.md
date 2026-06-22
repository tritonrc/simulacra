# R002 — Red-Green TDD (Multi-Model)

## Rule

Tests are written by a **different model** than the implementation. This is non-negotiable adversarial design — the test author and the implementer have different blind spots.

### Who Does What

| Phase | Model | Tool | Responsibility |
|---|---|---|---|
| Red (tests) | GPT-5.4 | `copilot --model gpt-5.4` | Write failing tests from spec |
| Green (impl) | Claude Code | sub-agent | Make tests pass |
| Refactor | Any | `cargo clippy` / `cargo fmt` | Mechanical cleanup |

**Claude MUST NOT write tests.** Claude makes the red tests green. If Claude writes tests, delete them and redo the red phase via copilot.

### Red Phase — Exact Command

```bash
copilot --model gpt-5.4 --allow-all --prompt "You are the RED team test writer for \
  the Simulacra Rust project. Read specs/S00N-*.md for the assertions to cover. \
  Read rules/R004-test-against-fakes.md for test patterns. \
  Read the existing code in crates/simulacra-<crate>/src/ for types and traits. \
  Write #[test] functions covering every '- [ ]' assertion in the spec. \
  Tests must compile but FAIL (no implementation yet). \
  Focus on edge cases, boundary conditions, and adversarial inputs. \
  Output only Rust test code."
```

### Green Phase

A Claude sub-agent receives:
1. The spec (`specs/S00N-*.md`)
2. The failing tests (already in the codebase from the red phase)
3. The relevant rules

It implements until `cargo test -p simulacra-<crate>` passes. It does NOT write new tests.

### Gates

1. **After red:** tests compile, tests fail. If any test passes without implementation, the test is wrong — fix it.
2. **After green:** `cargo test -p simulacra-<crate>` — all tests pass.
3. **After refactor:** all 4 mechanical checks pass (build, test, clippy, fmt).

### Test Requirements

1. Every spec assertion (`- [ ]` in `specs/S00N-*.md`) must have a corresponding `#[test]`.
2. Unit tests go in the same file (`#[cfg(test)] mod tests`). Integration tests go in `tests/`.
3. Tests use fakes per R004: `MemoryFs`, `InMemoryJournalStorage`, recorded HTTP fixtures.
4. Property-based tests (`proptest`) for parsers and path resolution.
5. Tests must be deterministic — no timing dependencies, no network, no real filesystem.

### Spec-Driven, Not Implementation-Driven

- Write tests **from the spec**, not from the implementation.
- If a test is hard to write because the spec is unclear, fix the spec first.
- If a test passes without implementation, the test is wrong.

## Why

Model diversity catches different classes of bugs. The test author doesn't know the implementation shortcuts — they test the contract, not the code. This produces tests that survive refactoring, because they were written against the spec, not against the current implementation. GPT-5.4 and Claude have different training data, different reasoning patterns, and different blind spots. Their disagreements surface bugs that self-testing never would.
