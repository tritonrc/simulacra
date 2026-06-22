# S027 — JS Agent Capabilities

**Status:** Active
**Crates involved:** `simulacra-quickjs`

## Dependencies

- **S003** — QuickJS runtime (host function contracts, module system)

## Scope

Fill capability gaps in the QuickJS runtime that prevent agents from performing common operations. Four tiers: web-standard globals, path module, crypto module, fs completions. All pure computation or VFS-backed.

Full spec: `specs/S027-js-agent-capabilities.md`

## Design

### Tier 1: Web-standard globals (pure computation)

JS polyfills and lightweight Rust-backed functions registered during runtime init. No host callbacks, no I/O.

- **`atob`/`btoa`** — base64 encode/decode. Rust-backed for correctness (WHATWG Latin1 restriction on `btoa`).
- **`TextEncoder`/`TextDecoder`** — UTF-8 only. Rust-backed via `encode()`/`decode()` on `String`/`[u8]`.
- **`URL`/`URLSearchParams`** — JS polyfill registered via `evalModule`. Handles http/https, query strings, hash. Not a full WHATWG URL parser.
- **`structuredClone`** — `JSON.parse(JSON.stringify(obj))` one-liner. Documented limitation: no `Date`, `RegExp`, `Map`, `Set`, circular refs.
- **`queueMicrotask`** — `Promise.resolve().then(fn)`.
- **`performance.now()`** — Rust `Instant` exposed as host function. Epoch is runtime start.
- **`setTimeout`/`clearTimeout`** — 0ms only (microtask scheduling). Non-zero delays clamped to 0. ID-based cancellation.
- **`console.error`/`warn`/`info`/`debug`** — Same formatting as `console.log`, with level metadata.

### Tier 2: `simulacra:path` — POSIX path manipulation

Native `ModuleDef` (Rust, per S016 pattern). Maps directly to `std::path::Path` operations. VFS is POSIX-only so no Windows paths.

Functions: `join`, `resolve`, `dirname`, `basename`, `extname`, `normalize`, `isAbsolute`, `relative`, `parse`, `format`. Constants: `sep = "/"`, `delimiter = ":"`.

`resolve` uses VFS cwd (not host cwd).

### Tier 3: `simulacra:crypto` — randomness and hashing

Native `ModuleDef` (Rust). Backed by `uuid`, `rand`, `sha2`, `md5` crates.

- `randomUUID()` — UUID v4 via `uuid::Uuid::new_v4()`.
- `randomBytes(n)` — `rand::thread_rng().fill()` into `Uint8Array`.
- `createHash(algo)` — returns JS object wrapping Rust hasher. `.update(data).digest(encoding)` pattern. Algorithms: sha256, sha512, md5.
- `getRandomValues(typedArray)` — Web Crypto compatible. 65536-byte limit.

### Tier 4: fs module completions

Extend existing `fs` module with missing VFS operations. All delegate through AgentCell proxy for capability enforcement (S011). No new capability types.

- `readdirSync(path)` — `VirtualFs::list_dir`. Returns `string[]`.
- `statSync(path)` — `VirtualFs::metadata`. Returns `{ isFile, isDirectory, size }`.
- `unlinkSync(path)` — `VirtualFs::delete`. File only.
- `renameSync(old, new)` — `VirtualFs::rename`. Creates parent dirs.
- `appendFileSync(path, data)` — Read + write at VFS level. Creates file if absent.

## Implementation notes

- Tier 1 items are independent of each other. Can be implemented in parallel.
- `simulacra:path` and `simulacra:crypto` follow the existing `ModuleDef` pattern from S016.
- Tier 4 functions follow the same host-function callback pattern as existing `fs.readFileSync`/`fs.writeFileSync`.
- No new crate dependencies beyond what is already in the workspace (uuid, rand, sha2, md5 are either present or trivially small).
