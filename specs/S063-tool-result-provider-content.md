# S063 — Provider Content Blocks on Tool Results

**Status:** Active — proposed
**Crates involved:** `simulacra-types`, `simulacra-tool`, `simulacra-runtime`, `simulacra-provider`

## Dependencies

- **ARCHITECTURE.md** — the journal is the source of truth; replay must
  reproduce the request the live turn made
- **S049** — the agent turn runtime this threads through
- **S050** — `Message.provider_content` and the `ProviderContentBlock`
  round-trip contract, added for `thinking` blocks on assistant messages

## Why this spec exists

A tool can only answer in text. `ToolOutput.content` is a `String`,
`execute_tool_live` reduces the whole output to `(String, bool)`
(`agent_loop/tool_execution.rs:4-14`), the tool `Message` is built with
`provider_content: vec![]` hardcoded (`agent_loop/turn/tools.rs:205-211`),
and the Anthropic adapter's `Role::Tool` branch serializes `msg.content` and
nothing else (`anthropic/api_types.rs:347-358`).

Anthropic's Messages API accepts `image` blocks inside `tool_result`
content — it is how the computer-use toolset returns screenshots. A host
whose tool has an image to show the model has no way to say so. The image
is dropped before it reaches the provider, silently, at four separate
boundaries.

S050 already created the vocabulary for this: `ProviderContentBlock` is a
provider-tagged opaque `serde_json::Value` that the runtime carries without
interpreting, and `Message.provider_content` is where such blocks live.
Assistant messages use it for `thinking`. Tool messages do not use it at
all. This spec makes them.

## Terminology

- **Tool result blocks** — `ProviderContentBlock`s a tool attaches to its
  output, to be sent alongside the text inside that tool's `tool_result`.
- **The text** — `ToolOutput.content`, unchanged in meaning: the
  model-visible text, and the only thing the journal's activity stream and
  `log_preview` reflect.

## Contract

### `ToolOutput` carries blocks

```rust
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub log_preview: String,
    pub structured: Option<serde_json::Value>,
    pub hook_input: Option<serde_json::Value>,
    pub hook_output: Option<serde_json::Value>,
    /// Provider-native blocks sent inside this result alongside `content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_content: Vec<ProviderContentBlock>,
}
```

`from_value` and `to_value` are hand-written (`tool.rs:155-200`) and must
carry the field, because `simulacra-tool`'s registry round-trips every
result through them (`registry.rs:251-263`). A host that constructs
`ToolOutput` directly through `Tool::output_from_value` is unaffected by
that, but a host that returns a raw object must not have its blocks
dropped by the conversion.

`success`, `error`, and every other constructor produce an empty vector.

### The runtime threads them through, unchanged

The blocks travel with the text from execution to the provider request:

1. `execute_tool_live` returns them beside `(content, is_error)`. Every
   path that produces a result without running a tool — capability
   denial, execution error — produces an empty vector.
2. `ToolExecutionResult` carries them. The `cancelled()` constructor and
   the approval-denied construction in `turn/tools.rs` produce an empty
   vector.
3. The tool `Message` carries them in `provider_content` instead of
   `vec![]`.
4. The `JournalEntryKind::ToolResult` entry records them, with
   `#[serde(default)]` so every existing journal still parses.
5. Replay restores them from the entry: `replay_tool_result`,
   `take_replay_tool_result`, and `consume_replay_tool_result_at` carry
   them beside `(content, is_error)`, so a replayed turn builds the same
   `Message` the live one did.

The runtime does not inspect a block's `value`. It is opaque here exactly
as it is for assistant `thinking` blocks.

### The Anthropic adapter emits them

`ApiRequestContentBlock::ToolResult.content` becomes text-or-blocks — the
same `#[serde(untagged)]` shape `ApiMessageContent` already uses:

```rust
ToolResult {
    tool_use_id: String,
    content: ToolResultContent,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
},

#[serde(untagged)]
enum ToolResultContent {
    Text(String),
    Blocks(Vec<ApiRequestContentBlock>),
}
```

A new request block variant carries the image:

```rust
#[serde(rename = "image")]
Image { source: serde_json::Value },
```

`source` is passed through as the host supplied it. The adapter does not
know or care whether it is a `file`, `base64`, or `url` source; that is
the host's contract with Anthropic, not the adapter's.

The `Role::Tool` branch: when the message's `provider_content` has no
`anthropic` block whose `type` is `image`, the wire form is **byte-identical
to today** — `content` is the plain string. When it has one or more, the
content is a block array: one `text` block carrying `msg.content` (omitted
when empty), then the image blocks in order. Non-`anthropic` blocks and
`anthropic` blocks of any other type are ignored on this branch.

`anthropic_provider_blocks` is **not** extended to accept `image`. It
serves the assistant branch; an assistant message has no caller producing
image blocks, and widening it would let one through silently.

## Non-goals

- **No `document`, `base64`, or other block types by name.** The adapter
  passes `source` through; which sources work is Anthropic's rule.
- **No user-role blocks.** `Role::User` stays coerced to text.
- **No change to `is_error` on the wire.** The `Role::Tool` branch hardcodes
  `is_error: false` today and signals errors through an `ERROR: ` text
  prefix. That is a pre-existing gap, noted, not fixed here.
- **No activity-stream or `log_preview` change.** Those reflect the text.
- **No persistence of blocks by any host.** Whether a host keeps them
  across turns is the host's business.

## Assertions

- [ ] A `ToolOutput` with non-empty `provider_content` survives
  `to_value` → `from_value` with the blocks intact and in order; one with
  empty `provider_content` serializes with no `provider_content` key and
  deserializes from a value lacking the key.
- [ ] A registered tool whose raw value carries `provider_content` reaches
  `ToolRegistry::call_output` with the blocks intact.
- [ ] A tool returning image blocks produces a `Role::Tool` `Message` whose
  `provider_content` holds those blocks and whose `content` is the text;
  the journal `ToolResult` entry for that call records the same blocks.
- [ ] Capability denial, execution error, cancellation, and approval denial
  each produce a tool `Message` with empty `provider_content`.
- [ ] Replaying a journal whose `ToolResult` entry carries blocks builds a
  `Message` with the same `provider_content` as the live turn did; a
  journal entry written before this spec (no `provider_content` key)
  replays with an empty vector.
- [ ] The Anthropic request for a tool message with no image blocks is
  byte-identical to the request built before this change.
- [ ] The Anthropic request for a tool message with `n` `anthropic` image
  blocks carries a `tool_result` whose `content` is an array of one `text`
  block followed by `n` `image` blocks, each with its `source` passed
  through unchanged; a message whose text is empty carries only the image
  blocks.
- [ ] A non-`anthropic` block, and an `anthropic` block of a type other than
  `image`, on a tool message do not appear in the request.
- [ ] An `image` block on an *assistant* message does not appear in the
  request.
