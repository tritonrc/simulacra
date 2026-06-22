# R001 — Dependency Management

## Rule

Before adding any new cargo dependency:

1. Check: Is it actively maintained (commits in last 6 months)?
2. Check: Does it have >1,000 downloads on crates.io?
3. Check: Could we write the needed functionality in <200 lines ourselves?
4. Check: What transitive dependencies does it pull in?

If the answer to #3 is yes, write it ourselves.

## Exceptions

The justified dependency list is in `ARCHITECTURE.md`. Adding to that list requires updating the architecture doc and noting the justification.

## Why

A dependency at the types/trait layer cascades through every crate. A dead dependency at the foundation layer is a project-wide migration. Our own 200-line implementation is always fork-free.
