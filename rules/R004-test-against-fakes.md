# R004 — Test Against Fakes

## Rule

- Use `MemoryFs` for all VFS tests. Never touch the real filesystem.
- Use `InMemoryJournalStorage` for all journal tests.
- Use recorded HTTP fixtures for provider tests. Never make live API calls in CI.
- **Exception:** E2E integration tests may use localhost TCP servers (`127.0.0.1:0`) to verify real HTTP paths (proxy, module fetch). These are self-contained, deterministic, and offline-safe.
- Test traits, not concrete types. Write the test against `&dyn VirtualFs`, not `&MemoryFs`.
- Use `proptest` for path resolution edge cases and shell parsing.
- **Never use source-scanning tests.** Tests that use `include_str!` to load source code and assert that specific strings or comments exist are not tests. They verify documentation was pasted into comments, not that code works. Every `#[test]` must exercise actual code paths — construct real objects, call real functions, assert on real outputs. If a spec assertion can't be tested with the current infrastructure, flag it as untestable rather than faking coverage with string matching.

## Why

Tests must be fast, deterministic, and runnable offline. Flaky tests that depend on network or filesystem state are worse than no tests. Tests that verify comments exist in source code provide zero confidence in correctness and create false positives.
