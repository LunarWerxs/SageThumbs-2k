#!/usr/bin/env node
// sign-via-mcp.mjs - sign files through an MCP server's `sign_artifact` tool, for
// sign-release.ps1's "mcp" backend.
//
// WHY: the signtool backend needs the signing service principal's secret in the BUILD
// process's environment. A signing MCP tool keeps it in the server's own vault and leases it
// into signtool alone, so the build never holds it. Connections Studio's local MCP ships
// such a tool (`sign_artifact`: Azure Artifact Signing, verified with Get-AuthenticodeSignature
// before it reports success). This script is the whole client: spawn the server over stdio,
// call the tool once for every file, and exit 0 ONLY when the server says every file was
// signed AND verified. sign-release.ps1 then reads each signature back through Windows itself.
//
// CONFIGURATION (none of it secret):
//   ST2K_SIGN_MCP              JSON array: the server's command line, e.g.
//                              ["node","C:/path/to/local-mcp/loader.mjs"]
//   ST2K_SIGN_MCP_INSTANCE     optional: which vaulted signing credential the tool leases
//   ST2K_SIGN_EXPECT_SUBJECT   optional: refuse unless the signer subject contains this
//
// Usage: node sign-via-mcp.mjs <file> [<file> ...]
// Exit:  0 every file signed and verified by the server; 1 anything else, with the reason.

import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { existsSync } from "node:fs";

const TOOL = "sign_artifact";
const TIMEOUT_MS = 20 * 60 * 1000;

function fail(msg) {
  console.error(`sign-via-mcp: ${msg}`);
  process.exit(1);
}

const files = process.argv.slice(2).map((f) => resolve(f));
if (!files.length) fail("no files named.");
for (const f of files) if (!existsSync(f)) fail(`no such file: ${f}`);

let command;
try {
  command = JSON.parse(process.env.ST2K_SIGN_MCP || "");
} catch {
  fail('ST2K_SIGN_MCP must be a JSON array holding the MCP server command line, e.g. ["node","C:/path/loader.mjs"].');
}
if (!Array.isArray(command) || !command.length || command.some((p) => typeof p !== "string"))
  fail("ST2K_SIGN_MCP must be a non-empty JSON array of strings.");

const params = { file: files };
if (process.env.ST2K_SIGN_MCP_INSTANCE) params.instance = process.env.ST2K_SIGN_MCP_INSTANCE;
if (process.env.ST2K_SIGN_EXPECT_SUBJECT) params.expect_subject = process.env.ST2K_SIGN_EXPECT_SUBJECT;
params.description = "SageThumbs 2K";

const child = spawn(command[0], command.slice(1), { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
child.on("error", (e) => fail(`could not start the MCP server (${command.join(" ")}): ${e.message}`));
child.stderr.on("data", () => {}); // server logs are not ours to parse; drain so it never blocks

const pending = new Map();
let nextId = 1;
let buffer = "";
child.stdout.setEncoding("utf8");
child.stdout.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line.startsWith("{")) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.id != null && pending.has(msg.id)) {
      const { ok, ko } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) ko(new Error(`${msg.error.code}: ${msg.error.message}`));
      else ok(msg.result);
    }
  }
});
child.on("exit", (code) => {
  for (const { ko } of pending.values()) ko(new Error(`the MCP server exited (code ${code}) before answering`));
  pending.clear();
});

function request(method, params) {
  const id = nextId++;
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  return new Promise((ok, ko) => pending.set(id, { ok, ko }));
}

const timer = setTimeout(() => {
  child.kill();
  fail(`no answer within ${TIMEOUT_MS / 60000} minutes; nothing is reported as signed.`);
}, TIMEOUT_MS);

try {
  await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "sagethumbs2k-sign-release", version: "1" },
  });
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
  const listed = await request("tools/list", {});
  const names = new Set((listed?.tools || []).map((t) => t.name));
  // A server may expose the tool directly, or (Connections) behind one execute door that
  // runs any of its local tools by name. Anything else cannot sign.
  let call;
  if (names.has(TOOL)) call = { name: TOOL, arguments: params };
  else if (names.has("connections_execute"))
    call = { name: "connections_execute", arguments: { local: true, tool_name: TOOL, params } };
  else fail(`the MCP server exposes no ${TOOL} tool (it lists: ${[...names].join(", ") || "nothing"}).`);

  const result = await request("tools/call", call);
  const text = (result?.content || [])
    .filter((c) => c?.type === "text")
    .map((c) => c.text)
    .join("\n");
  console.log(text);
  // The tool's own success headline, and ONLY that, counts. Its failure texts, a refusal
  // naming a missing credential, and an isError result all fall through to exit 1.
  const m = /sign_artifact: signed and verified (\d+) file/.exec(text);
  if (result?.isError || !m || Number(m[1]) !== files.length)
    fail(`the server did not report all ${files.length} file(s) signed and verified; treat them as UNSIGNED.`);
  clearTimeout(timer);
  child.kill();
  process.exit(0);
} catch (e) {
  fail(e?.message || String(e));
}
