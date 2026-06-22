#!/usr/bin/env bun
/**
 * Two-Simulacras Bridge
 *
 * Watches Acme and Maya's SQLite memory stores for outbox messages,
 * routes them between instances, and renders a live "Slack" view in the terminal.
 *
 * Architecture:
 *   ┌──────────┐   outbox  ┌─────────┐  outbox  ┌──────────┐
 *   │  Acme    │ ────────► │ Bridge  │ ◄──────── │  Maya    │
 *   │  simulacra   │ ◄──────── │  (Bun)  │ ────────► │  simulacra   │
 *   └──────────┘  (task)   └─────────┘  (task)   └──────────┘
 *
 * One-shot model: each simulacra invocation is a fresh process; state lives in SQLite.
 */

import { Database } from "bun:sqlite";
import { existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dir = path.dirname(fileURLToPath(import.meta.url));

// ── Config ─────────────────────────────────────────────────────────────────
const SIMULACRA_BIN = process.env.SIMULACRA_BIN ?? path.join(__dir, "../../target/debug/simulacra");
const ACME_CONFIG = path.join(__dir, "acme/simulacra.toml");
const MAYA_CONFIG = path.join(__dir, "maya/simulacra.toml");
const ACME_DB = path.join(__dir, "acme-mem/memory/acme.db");
const MAYA_DB = path.join(__dir, "maya-mem/memory/maya.db");

// SQLite LIKE pattern for /var/memory/outbox/ entries
const OUTBOX_LIKE = "/var/memory/outbox/%";

const MAX_ROUNDS = 8;

// ── ANSI helpers ───────────────────────────────────────────────────────────
const R = "\x1b[0m";
const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const BLUE = "\x1b[94m";   // Acme
const AMBER = "\x1b[33m";  // Maya
const GREEN = "\x1b[92m";
const GRAY = "\x1b[90m";

function hr(char = "─", n = 62) { return char.repeat(n); }

function showBanner() {
  console.log(`\n${BOLD}${hr("═")}${R}`);
  console.log(`${BOLD}  🏢  Two-Simulacras Demo — Acme Corp  ×  Maya Chen${R}`);
  console.log(`${BOLD}${hr("═")}${R}\n`);
}

function showMessage(side: "acme" | "maya", content: string, filename: string) {
  const ts = new Date().toLocaleTimeString("en-US", { hour12: false });
  const header =
    side === "acme"
      ? `${BLUE}${BOLD}💼  Acme Corp${R} ${GRAY}· ${filename} · ${ts}${R}`
      : `${AMBER}${BOLD}👩  Maya Chen${R} ${GRAY}· ${filename} · ${ts}${R}`;
  console.log(`\n${header}`);
  console.log(DIM + hr() + R);
  console.log(content.trim());
}

function log(msg: string) {
  console.log(`\n${GRAY}⟳  ${msg}${R}`);
}

// ── SQLite polling ─────────────────────────────────────────────────────────
//
// `version` in memory_content is per-path (not a global counter), so we
// can't use it as a cursor. Instead we track delivered paths in a Set.
interface Row { path: string; data: Uint8Array }

function pollOutbox(
  dbPath: string,
  delivered: Set<string>
): Array<{ path: string; content: string }> {
  if (!existsSync(dbPath)) return [];
  // Open without readonly — Bun's sqlite SQLITE_CANTOPEN on WAL-mode files
  // opened readonly; we never write through this handle.
  const db = new Database(dbPath);
  try {
    const rows = db
      .query<Row, [string]>(
        `SELECT path, data FROM memory_content
         WHERE path LIKE ? AND deleted = 0
         ORDER BY mtime_ns ASC`
      )
      .all(OUTBOX_LIKE);
    return rows
      .filter((r) => !delivered.has(r.path))
      .map((r) => ({
        path: r.path,
        content: new TextDecoder().decode(r.data),
      }));
  } finally {
    db.close();
  }
}

// ── Simulacra runner ───────────────────────────────────────────────────────────
const OTLP_ENDPOINT = process.env.OTLP_ENDPOINT ?? "http://localhost:4320";

async function runSimulacra(config: string, task: string): Promise<void> {
  const side = config.includes("acme") ? "acme" : "maya";
  log(`Invoking simulacra [${side}]…`);

  const args = [SIMULACRA_BIN, "--config", config, "--task", task,
    "--otlp-endpoint", OTLP_ENDPOINT];

  const proc = Bun.spawn(args, {
    cwd: __dir,
    stdout: "pipe",
    stderr: "inherit", // surface errors directly
    env: { ...process.env },
  });

  // Stream stdout dimmed so the "Slack" messages stand out
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    const text = decoder.decode(value, { stream: true });
    if (text.trim()) process.stdout.write(DIM + text + R);
  }

  const code = await proc.exited;
  if (code !== 0) {
    console.error(`${AMBER}[simulacra:${side}] exited with code ${code}${R}`);
  }
}

// ── Success detection ──────────────────────────────────────────────────────
const SUCCESS_SIGNALS = [
  "take my money",
  "sign me up",
  "where do i sign",
  "how do i pay",
  "ready to buy",
  "ready to purchase",
  "i'll buy",
  "i want to buy",
  "i'm in",
  "let's do it",
  "sold!",
  "count me in",
  "send me an invoice",
  "interested in purchasing",
];

function isSuccess(content: string): boolean {
  const lc = content.toLowerCase();
  return SUCCESS_SIGNALS.some((s) => lc.includes(s));
}

// ── Pre-flight ─────────────────────────────────────────────────────────────
function preflight(): void {
  if (!existsSync(SIMULACRA_BIN)) {
    console.error(`\nSimulacra binary not found at: ${SIMULACRA_BIN}`);
    console.error("Run ./run.sh (which builds it) or set SIMULACRA_BIN env var.\n");
    process.exit(1);
  }
  if (!process.env.ANTHROPIC_API_KEY) {
    console.error("\nANTHROPIC_API_KEY is not set. Export it before running.\n");
    process.exit(1);
  }
  // Ensure memory dirs exist before first pollOutbox check
  mkdirSync(path.join(__dir, "acme-mem"), { recursive: true });
  mkdirSync(path.join(__dir, "maya-mem"), { recursive: true });
}

// ── Main ───────────────────────────────────────────────────────────────────
async function main() {
  preflight();
  showBanner();

  const acmeDelivered = new Set<string>();
  const mayaDelivered = new Set<string>();

  // Kickoff: Acme reaches out first
  const kickoff = `\
You have a warm lead: Maya Chen, co-founder of Veritas Labs (5-person startup), is looking for task management software. She was referred by a mutual contact.

ACTION REQUIRED: Use the file_write tool to write your opening message to /var/memory/outbox/msg-01.md

The file content should be your actual customer-facing message — short (3-4 sentences max), professional, and ending with one specific question about her team's current workflow. Do not explain what you are writing — just write it using file_write.`;

  await runSimulacra(ACME_CONFIG, kickoff);

  // Conversation loop
  for (let round = 1; round <= MAX_ROUNDS; round++) {
    log(`Round ${round}/${MAX_ROUNDS}`);

    // ── Acme → Maya ──────────────────────────────────────────────────────
    const acmeMsgs = pollOutbox(ACME_DB, acmeDelivered);
    if (acmeMsgs.length === 0) {
      log("Acme wrote no outbox message — ending.");
      break;
    }

    // Take the last message written this turn (in case of multiple)
    const acmeMsg = acmeMsgs[acmeMsgs.length - 1];
    acmeDelivered.add(acmeMsg.path);
    showMessage("acme", acmeMsg.content, path.basename(acmeMsg.path));

    // Deliver to Maya
    const mayaTask = `\
You received a message from Acme Corp:

---
${acmeMsg.content}
---

You are Maya Chen. React to what they actually said.

ACTION REQUIRED: Use the file_write tool to write your response to /var/memory/outbox/msg-${String(round).padStart(2, "0")}.md

Write the file content as your direct reply — push back on anything vague, ask one concrete follow-up question. Do not explain what you are about to write — just write it using file_write.`;

    await runSimulacra(MAYA_CONFIG, mayaTask);

    // ── Maya → Acme ──────────────────────────────────────────────────────
    const mayaMsgs = pollOutbox(MAYA_DB, mayaDelivered);
    if (mayaMsgs.length === 0) {
      log("Maya wrote no outbox message — ending.");
      break;
    }

    const mayaMsg = mayaMsgs[mayaMsgs.length - 1];
    mayaDelivered.add(mayaMsg.path);
    showMessage("maya", mayaMsg.content, path.basename(mayaMsg.path));

    if (isSuccess(mayaMsg.content)) {
      console.log(`\n${GREEN}${BOLD}🎉  Maya is ready to buy! Conversation complete.${R}\n`);
      return;
    }

    if (round === MAX_ROUNDS) break;

    // Build Acme's next task
    const acmeTask = `\
Maya Chen responded:

---
${mayaMsg.content}
---

You are the Acme product team. Read what she said and respond directly.

If she asked for a specific feature or demo:
  - Write the JavaScript code and execute it NOW using the javascript tool
  - Then write your message (including the actual code output) using file_write

ACTION REQUIRED: Use the file_write tool to write your response to /var/memory/outbox/msg-${String(round + 1).padStart(2, "0")}.md

Do not explain what you plan to write — just write it. The file content is your customer-facing message.`;

    await runSimulacra(ACME_CONFIG, acmeTask);
  }

  console.log(`\n${AMBER}Demo finished after ${MAX_ROUNDS} rounds.${R}\n`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
