// Tests for scripts/packaging/sign-via-mcp.mjs, run by `node --test` (scripts/test-script-tests.ps1).
// A stub MCP server stands in for Connections: it answers `sign_artifact` the way the real one
// does, and like the real one its read-back fails when it inherits PowerShell 7's PSModulePath.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const SIGNER = join(dirname(fileURLToPath(import.meta.url)), 'packaging', 'sign-via-mcp.mjs');

const STUB = `
let buf = '';
const send = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\\n');
process.stdin.setEncoding('utf8');
process.stdin.on('data', (c) => {
  buf += c; let nl;
  while ((nl = buf.indexOf('\\n')) >= 0) {
    const m = JSON.parse(buf.slice(0, nl)); buf = buf.slice(nl + 1);
    if (m.method === 'initialize') send(m.id, { protocolVersion: '2025-06-18', capabilities: {} });
    else if (m.method === 'tools/list') send(m.id, { tools: [{ name: 'sign_artifact' }] });
    else if (m.method === 'tools/call') {
      const inherited = Object.keys(process.env).some((k) => k.toUpperCase() === 'PSMODULEPATH');
      const n = m.params.arguments.file.length;
      send(m.id, { content: [{ type: 'text', text: inherited
        ? 'sign_artifact: could not read the signature back - the module could not be loaded'
        : 'sign_artifact: signed and verified ' + n + ' file(s)' }] });
    }
  }
});
`;

test('the signing server starts without PowerShell 7\'s PSModulePath', () => {
  const dir = mkdtempSync(join(tmpdir(), 'st2k-sign-mcp-'));
  try {
    const stub = join(dir, 'stub-server.mjs');
    const target = join(dir, 'a.dll');
    writeFileSync(stub, STUB);
    writeFileSync(target, 'MZ');
    const r = spawnSync(process.execPath, [SIGNER, target], {
      encoding: 'utf8',
      timeout: 60_000,
      env: {
        ...process.env,
        PSModulePath: 'C:\\Program Files\\PowerShell\\7\\Modules',
        ST2K_SIGN_MCP: JSON.stringify([process.execPath, stub]),
      },
    });
    assert.equal(r.status, 0, `signer failed:\n${r.stdout}\n${r.stderr}`);
    assert.match(r.stdout, /signed and verified 1 file/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
