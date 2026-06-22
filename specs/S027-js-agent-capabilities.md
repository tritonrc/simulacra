# S027 — JS Agent Capabilities

**Status:** Active
**Crate:** `simulacra-quickjs`

## Dependencies

- **S003** — QuickJS runtime (host function contracts, module system)

## Scope

Fill capability gaps in the QuickJS runtime that prevent agents from performing common operations. All additions are pure computation or VFS-backed — no new I/O surfaces, no new capability requirements.

**In scope:**
- Tier 1: Web-standard globals — `atob`/`btoa`, `TextEncoder`/`TextDecoder`, `URL`/`URLSearchParams`, `structuredClone`, `queueMicrotask`, `performance.now()`, `setTimeout`/`clearTimeout` (0ms only), console level methods
- Tier 2: `simulacra:path` module — POSIX path manipulation (join, resolve, dirname, basename, extname, normalize, isAbsolute, relative, parse, format)
- Tier 3: `simulacra:crypto` module — `randomUUID`, `randomBytes`, `createHash` (sha256/sha512/md5), `getRandomValues`
- Tier 4: fs module completions — `readdirSync`, `statSync`, `unlinkSync`, `renameSync`, `appendFileSync`

**Out of scope:**
- Real `setTimeout` with delays > 0ms (requires event loop changes)
- `setInterval` / `clearInterval` (requires event loop)
- Node.js `Buffer` class (use `TextEncoder`/`TextDecoder` + `Uint8Array`)
- Full Web Crypto API (`subtle.encrypt`, `subtle.sign`, etc.)
- `crypto.createCipheriv` / `crypto.createDecipheriv` (symmetric encryption)
- `fs.watchFile` / `fs.watch` (filesystem events)
- `fs.createReadStream` / `fs.createWriteStream` (streaming I/O)
- Non-POSIX path semantics (Windows `\` separators)

## Context

Simulacra's QuickJS runtime is the execution environment agents use via `js_exec`. The engine already provides JSON, regex, string ops, `fetch()`, basic fs (read/write/exists/mkdir), `process.env`, ESM modules, and `console.log`.

A gap analysis found that agents routinely need operations they cannot currently perform: base64 encoding for API payloads, URL parsing for HTTP operations, path manipulation for filesystem work, hashing for cache keys and content verification, and directory listing for exploring the workspace. These are all standard operations in any JS runtime (Node.js, Deno, browsers) and their absence forces agents into awkward workarounds or tool-call round-trips.

Every item in this spec is either pure computation (no I/O at all) or backed by the existing VFS layer (which already enforces capabilities via the AgentCell proxy per S011). No new I/O surfaces are introduced. No new capability types are required.

## Design

### Tier 1: Web-standard globals

Pure computation polyfills registered during runtime initialization. No host callbacks needed.

#### `atob(string)` / `btoa(string)`

Standard base64 encode/decode. `btoa` encodes a string to base64. `atob` decodes base64 to a string. Follows the WHATWG spec: `btoa` throws on characters outside Latin1 range.

#### `TextEncoder` / `TextDecoder`

`new TextEncoder()` — always UTF-8. `.encode(string)` returns `Uint8Array`.
`new TextDecoder(encoding?)` — defaults to UTF-8. `.decode(uint8array)` returns string. Only UTF-8 is required for S027; other encodings are out of scope.

#### `URL` / `URLSearchParams`

`new URL(input, base?)` — WHATWG URL parsing. Properties: `href`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin`, `username`, `password`, `searchParams`.
`new URLSearchParams(init?)` — init from string, object, or entries. `.get()`, `.set()`, `.append()`, `.delete()`, `.has()`, `.toString()`, `.entries()`, `.keys()`, `.values()`, `.forEach()`, `[Symbol.iterator]`.

Implemented as JS polyfill registered via `evalModule`. Does not need to handle every edge case of the WHATWG URL spec — common patterns (http/https URLs, query strings, hash fragments) must work correctly.

#### `structuredClone(obj)`

Deep clone via `JSON.parse(JSON.stringify(obj))`. This means it does not handle `Date`, `RegExp`, `Map`, `Set`, `ArrayBuffer`, `undefined` values in objects, or circular references. This is documented and acceptable — agents primarily clone plain JSON-serializable data.

#### `queueMicrotask(fn)`

`Promise.resolve().then(fn)` polyfill. Schedules `fn` to run after the current microtask completes.

#### `performance.now()`

Returns a high-resolution timestamp in milliseconds. Backed by Rust's `std::time::Instant`. Resolution is sub-millisecond. The epoch is arbitrary (runtime start), not wall-clock.

#### `setTimeout(fn, delay)` / `clearTimeout(id)`

For S027, only 0ms delay is supported. `setTimeout(fn, 0)` schedules `fn` as a microtask (same as `queueMicrotask`). Any non-zero delay is clamped to 0. `clearTimeout` cancels a pending timeout by ID. Returns a numeric ID.

This is intentionally limited. Real timer support requires event loop changes (future spec).

#### `console.error` / `console.warn` / `console.info` / `console.debug`

Level-aware variants of `console.log`. All use the same Node.js-style formatting (S003 behavior 5). The level is preserved in the output metadata:

- `console.error(...)` — level `ERROR`, writes to virtual stderr
- `console.warn(...)` — level `WARN`, writes to virtual stderr
- `console.info(...)` — level `INFO`, writes to virtual stdout
- `console.debug(...)` — level `DEBUG`, writes to virtual stdout

### Tier 2: `simulacra:path` module

A native Rust module exposed as `simulacra:path`. Imported via `import path from 'simulacra:path'` or `import { join, resolve } from 'simulacra:path'`.

All operations are POSIX-only. The VFS is POSIX, so Windows path semantics are irrelevant.

Implemented in Rust as a `ModuleDef` (per S016 native module pattern). Each function is a direct mapping to Rust's `std::path::Path` operations with POSIX normalization.

| Function | Behavior |
|----------|----------|
| `path.join(...segments)` | Concatenate segments with `/`, resolve `..` and `.` |
| `path.resolve(...segments)` | Resolve to absolute path against VFS cwd |
| `path.dirname(p)` | Parent directory (`/a/b/c` → `/a/b`) |
| `path.basename(p, ext?)` | Final component, optionally strip suffix |
| `path.extname(p)` | Extension including dot (`file.tar.gz` → `.gz`) |
| `path.normalize(p)` | Collapse `.`, `..`, multiple slashes |
| `path.isAbsolute(p)` | Returns `true` if starts with `/` |
| `path.relative(from, to)` | Relative path from `from` to `to` |
| `path.parse(p)` | Returns `{ root, dir, base, ext, name }` |
| `path.format(obj)` | Inverse of `parse` — object to path string |
| `path.sep` | Always `"/"` |
| `path.delimiter` | Always `":"` |

### Tier 3: `simulacra:crypto` module

A native Rust module exposed as `simulacra:crypto`. Imported via `import crypto from 'simulacra:crypto'`.

Backed by the `uuid`, `rand`, and `sha2`/`md5` crates (already in the dependency tree or trivially addable).

| Function | Behavior |
|----------|----------|
| `crypto.randomUUID()` | Returns a UUID v4 string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`) |
| `crypto.randomBytes(n)` | Returns `Uint8Array` of `n` cryptographically random bytes |
| `crypto.createHash(algo)` | Returns a `Hash` object. Supported algorithms: `sha256`, `sha512`, `md5` |
| `crypto.getRandomValues(typedArray)` | Fills typed array with random values, returns it. Web Crypto compatible. |

#### Hash object

`crypto.createHash("sha256")` returns an object with:
- `.update(data)` — feed string data. Returns `this` for chaining.
- `.digest(encoding?)` — finalize and return hash. `encoding` is `"hex"` (default) or `"base64"`. Without encoding, returns `Uint8Array`.

`createHash` throws on unsupported algorithm names.

### Tier 4: fs module completions

These extend the existing `fs` module (S003). Each function delegates through the AgentCell proxy, which enforces capabilities per S011. No new capability types needed — these use existing read/write/delete permissions.

| Function | VFS operation | Capability required |
|----------|---------------|-------------------|
| `fs.readdirSync(path)` | `list_dir` | `fs:read` on path |
| `fs.statSync(path)` | `metadata` | `fs:read` on path |
| `fs.unlinkSync(path)` | `delete` | `fs:write` on path |
| `fs.renameSync(old, new)` | `rename` | `fs:write` on both paths |
| `fs.appendFileSync(path, data)` | `read` + `write` | `fs:write` on path |

#### `fs.readdirSync(path)`

Returns an array of entry names (strings) in the directory. Does not include `.` or `..`. Throws if path does not exist or is not a directory.

#### `fs.statSync(path)`

Returns an object: `{ isFile: bool, isDirectory: bool, size: number }`. `size` is in bytes. Throws if path does not exist. This is a simplified stat — no timestamps, permissions, or inode info (VFS does not track these).

#### `fs.unlinkSync(path)`

Deletes a file. Throws if path does not exist or is a directory (use `fs.rmdirSync` for directories, out of scope for S027).

#### `fs.renameSync(oldPath, newPath)`

Moves/renames a file or directory. Throws if `oldPath` does not exist. Creates parent directories of `newPath` if needed (consistent with `writeFileSync` behavior).

#### `fs.appendFileSync(path, data)`

Appends `data` to file. If file does not exist, creates it (consistent with Node.js behavior). Equivalent to `fs.writeFileSync(path, fs.readFileSync(path) + data)` but atomic at the VFS level.

## Behavior

### Tier 1: Web-standard globals

1. `btoa("hello")` returns `"aGVsbG8="`. `atob("aGVsbG8=")` returns `"hello"`.
2. `btoa` throws a `DOMException`-like error on characters with code points > 255.
3. `atob` throws on invalid base64 input (non-base64 characters, incorrect padding).
4. `new TextEncoder().encode("hello")` returns a `Uint8Array` of UTF-8 bytes `[104, 101, 108, 108, 111]`.
5. `new TextDecoder().decode(uint8array)` returns the UTF-8 decoded string.
6. `new URL("https://example.com/path?q=1#frag")` correctly parses all components.
7. `new URL("/path", "https://base.com")` resolves relative URLs against the base.
8. `URLSearchParams` supports get/set/append/delete/has/toString and iteration.
9. `structuredClone({a: {b: 1}})` returns a deep copy — mutating the clone does not affect the original.
10. `queueMicrotask(fn)` schedules `fn` to execute after the current task.
11. `performance.now()` returns a number. Two calls return non-decreasing values. Resolution is sub-millisecond.
12. `setTimeout(fn, 0)` schedules `fn` as a microtask. Returns a numeric ID.
13. `clearTimeout(id)` cancels a pending timeout.
14. `setTimeout(fn, 100)` clamps delay to 0 and executes as microtask (S027 limitation).
15. `console.error("msg")` writes to virtual stderr with ERROR level.
16. `console.warn("msg")` writes to virtual stderr with WARN level.
17. `console.info("msg")` writes to virtual stdout with INFO level.
18. `console.debug("msg")` writes to virtual stdout with DEBUG level.
19. All console level methods use the same formatting as `console.log` (S003 behavior 5).

### Tier 2: `simulacra:path` module

20. `path.join("a", "b", "c")` returns `"a/b/c"`.
21. `path.join("/a", "b", "..", "c")` returns `"/a/c"`.
22. `path.resolve("a", "b")` resolves against the VFS cwd to produce an absolute path.
23. `path.dirname("/a/b/c")` returns `"/a/b"`.
24. `path.basename("/a/b/c.txt")` returns `"c.txt"`. `path.basename("/a/b/c.txt", ".txt")` returns `"c"`.
25. `path.extname("file.tar.gz")` returns `".gz"`.
26. `path.normalize("/a//b/../c")` returns `"/a/c"`.
27. `path.isAbsolute("/a")` returns `true`. `path.isAbsolute("a")` returns `false`.
28. `path.relative("/a/b", "/a/c")` returns `"../c"`.
29. `path.parse("/a/b/c.txt")` returns `{ root: "/", dir: "/a/b", base: "c.txt", ext: ".txt", name: "c" }`.
30. `path.format({ dir: "/a/b", base: "c.txt" })` returns `"/a/b/c.txt"`.
31. `path.sep` is `"/"` and `path.delimiter` is `":"`.

### Tier 3: `simulacra:crypto` module

32. `crypto.randomUUID()` returns a string matching UUID v4 format.
33. Successive `randomUUID()` calls return different values.
34. `crypto.randomBytes(16)` returns a `Uint8Array` of length 16.
35. `crypto.randomBytes(0)` returns an empty `Uint8Array`.
36. `crypto.createHash("sha256").update("hello").digest("hex")` returns the correct SHA-256 hex digest.
37. `crypto.createHash("sha512").update("data").digest("base64")` returns the correct SHA-512 base64 digest.
38. `crypto.createHash("md5").update("test").digest("hex")` returns the correct MD5 hex digest.
39. `crypto.createHash("unknown")` throws an error.
40. `.update()` is chainable: `hash.update("a").update("b").digest("hex")` equals hashing `"ab"`.
41. `.digest()` without encoding returns a `Uint8Array`.
42. `crypto.getRandomValues(new Uint8Array(8))` fills the array with random bytes and returns it.
43. `crypto.getRandomValues` throws if the typed array exceeds 65536 bytes (Web Crypto limit).

### Tier 4: fs module completions

44. `fs.readdirSync("/workspace")` returns an array of entry names.
45. `fs.readdirSync` does not include `.` or `..` in results.
46. `fs.readdirSync` on a nonexistent path throws an error.
47. `fs.readdirSync` on a file (not directory) throws an error.
48. `fs.statSync("/workspace/file.txt")` returns `{ isFile: true, isDirectory: false, size: N }`.
49. `fs.statSync("/workspace")` returns `{ isFile: false, isDirectory: true, size: N }`.
50. `fs.statSync` on a nonexistent path throws an error.
51. `fs.unlinkSync("/workspace/file.txt")` deletes the file.
52. `fs.unlinkSync` on a nonexistent path throws an error.
53. `fs.unlinkSync` on a directory throws an error.
54. `fs.renameSync("/workspace/a.txt", "/workspace/b.txt")` moves the file.
55. `fs.renameSync` on a nonexistent source throws an error.
56. `fs.renameSync` creates parent directories of the destination if needed.
57. `fs.appendFileSync("/workspace/file.txt", "more")` appends to existing file content.
58. `fs.appendFileSync` creates the file if it does not exist.
59. All fs completions delegate through AgentCell proxy for capability enforcement (S011).

## Assertions

### Tier 1: Web-standard globals

- [x] `btoa("hello")` returns `"aGVsbG8="`.
- [x] `atob("aGVsbG8=")` returns `"hello"`.
- [x] `btoa` throws on code points > 255.
- [x] `atob` throws on invalid base64.
- [x] `btoa(atob(str))` roundtrips for valid base64 strings.
- [x] `new TextEncoder().encode("hello")` returns correct UTF-8 bytes.
- [x] `new TextDecoder().decode(encoded)` roundtrips with `TextEncoder`.
- [x] `new TextDecoder()` defaults to UTF-8.
- [x] `new URL(str)` parses protocol, host, pathname, search, hash correctly.
- [x] `new URL(relative, base)` resolves relative to base.
- [x] `url.searchParams` returns a working `URLSearchParams`.
- [x] `URLSearchParams` get/set/append/delete/has work correctly.
- [x] `URLSearchParams.toString()` produces correct query string.
- [x] `URLSearchParams` is iterable.
- [x] `structuredClone(obj)` produces a deep copy.
- [x] Mutating a `structuredClone` result does not affect the original.
- [x] `queueMicrotask(fn)` executes `fn` asynchronously.
- [x] `performance.now()` returns a number.
- [x] Two `performance.now()` calls return non-decreasing values.
- [x] `setTimeout(fn, 0)` executes `fn`.
- [x] `setTimeout` returns a numeric ID.
- [x] `clearTimeout(id)` prevents execution of the scheduled function.
- [x] `setTimeout(fn, 100)` clamps to 0ms and executes.
- [x] `console.error` writes to virtual stderr with ERROR level.
- [x] `console.warn` writes to virtual stderr with WARN level.
- [x] `console.info` writes to virtual stdout with INFO level.
- [x] `console.debug` writes to virtual stdout with DEBUG level.
- [x] All console methods format output per S003 behavior 5.

### Tier 2: `simulacra:path` module

- [x] `path.join("a", "b", "c")` returns `"a/b/c"`.
- [x] `path.join` resolves `..` and `.` segments.
- [x] `path.resolve` produces an absolute path against VFS cwd.
- [x] `path.dirname` returns the parent directory.
- [x] `path.basename` returns the final component.
- [x] `path.basename` with ext argument strips the suffix.
- [x] `path.extname` returns the extension including the dot.
- [x] `path.normalize` collapses `.`, `..`, and duplicate slashes.
- [x] `path.isAbsolute` returns `true` for `/`-prefixed paths.
- [x] `path.isAbsolute` returns `false` for relative paths.
- [x] `path.relative` computes the correct relative path.
- [x] `path.parse` returns `{ root, dir, base, ext, name }`.
- [x] `path.format` is the inverse of `path.parse`.
- [x] `path.sep` is `"/"`.
- [x] `path.delimiter` is `":"`.
- [x] `import path from 'simulacra:path'` works as default import.
- [x] `import { join } from 'simulacra:path'` works as named import.

### Tier 3: `simulacra:crypto` module

- [x] `crypto.randomUUID()` returns a valid UUID v4 string.
- [x] Successive `randomUUID()` calls return distinct values.
- [x] `crypto.randomBytes(16)` returns a `Uint8Array` of length 16.
- [x] `crypto.randomBytes(0)` returns an empty `Uint8Array`.
- [x] `crypto.createHash("sha256").update("hello").digest("hex")` returns correct digest.
- [x] `crypto.createHash("sha512").update("data").digest("base64")` returns correct digest.
- [x] `crypto.createHash("md5").update("test").digest("hex")` returns correct digest.
- [x] `crypto.createHash("unknown")` throws.
- [x] `.update()` returns `this` for chaining.
- [x] Multiple `.update()` calls are equivalent to hashing the concatenation.
- [x] `.digest()` without encoding returns `Uint8Array`.
- [x] `.digest("hex")` returns lowercase hex string.
- [x] `.digest("base64")` returns base64 string.
- [x] `crypto.getRandomValues(new Uint8Array(8))` fills and returns the array.
- [x] `crypto.getRandomValues` throws for typed arrays > 65536 bytes.
- [x] `import crypto from 'simulacra:crypto'` works as default import.
- [x] `import { randomUUID } from 'simulacra:crypto'` works as named import.

### Tier 4: fs module completions

- [x] `fs.readdirSync` returns array of entry names.
- [x] `fs.readdirSync` excludes `.` and `..`.
- [x] `fs.readdirSync` throws on nonexistent path.
- [x] `fs.readdirSync` throws on file path (not directory).
- [x] `fs.statSync` returns `{ isFile, isDirectory, size }` for a file.
- [x] `fs.statSync` returns `{ isFile, isDirectory, size }` for a directory.
- [x] `fs.statSync` throws on nonexistent path.
- [x] `fs.unlinkSync` deletes a file.
- [x] `fs.unlinkSync` throws on nonexistent path.
- [x] `fs.unlinkSync` throws on directory.
- [x] `fs.renameSync` moves a file.
- [x] `fs.renameSync` throws on nonexistent source.
- [x] `fs.renameSync` creates parent directories of destination.
- [x] `fs.appendFileSync` appends to existing file.
- [x] `fs.appendFileSync` creates file if nonexistent.
- [x] All fs completions enforce capabilities via AgentCell proxy.

## Observability (see S010)

- [x] `simulacra_js_exec` span already wraps all JS execution (S003). No new spans needed for Tier 1 globals.
- [x] `simulacra:crypto` hash operations are pure computation — no spans needed.
- [x] Tier 4 fs operations produce VFS spans via the existing AgentCell proxy (S011). No new o11y instrumentation needed.
- [x] `console.error` and `console.warn` output is captured at appropriate log levels in the agent's output stream.
- [x] `tracing::debug!` when `simulacra:path` or `simulacra:crypto` modules are loaded during runtime init.
