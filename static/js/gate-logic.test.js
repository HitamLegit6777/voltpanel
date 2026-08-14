"use strict";
/* Node-only fixture test for the Flow Gate condition logic in static/js/app.js.
   Extracts the DOM-free gate functions straight from app.js source and proves
   the roundtrip the edit flow relies on:

       backend condition (API JSON, sorted keys)
         -> form state (schCondFromValue / schEdit prepopulate)
         -> API body   (schCondBody / schSave)

   stays byte-identical, that unknown kinds are preserved instead of dropped,
   that client validation mirrors the backend bounds (validate_condition in
   src/services/scheduler.rs), and that the new gate_* i18n keys exist in both
   the en and id dictionaries.

   Run: node static/js/gate-logic.test.js   (exit 0 = pass)
*/
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const src = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");

/* --- source extraction (balanced braces, string/comment aware) --- */
function closingBrace(src, openIdx) {
  let depth = 0, i = openIdx;
  while (i < src.length) {
    const c = src[i], n = src[i + 1];
    if (c === "/" && n === "/") { while (i < src.length && src[i] !== "\n") i++; continue; }
    if (c === "/" && n === "*") { i += 2; while (i < src.length && !(src[i] === "*" && src[i + 1] === "/")) i++; i += 2; continue; }
    if (c === '"' || c === "'") { const q = c; i++; while (i < src.length && src[i] !== q) { if (src[i] === "\\") i++; i++; } i++; continue; }
    if (c === "`") { i++; while (i < src.length && src[i] !== "`") { if (src[i] === "\\") i++; else if (src[i] === "$" && src[i + 1] === "{") { const j = closingBrace(src, i + 1); i = j + 1; continue; } i++; } i++; continue; }
    if (c === "{") depth++;
    else if (c === "}") { depth--; if (depth === 0) return i; }
    i++;
  }
  throw new Error("unbalanced source");
}
function extractFunction(name) {
  const re = new RegExp(`\\bfunction ${name}\\s*\\(`);
  const m = re.exec(src);
  if (!m) throw new Error(`function ${name} not found in app.js`);
  const open = src.indexOf("{", m.index);
  return src.slice(m.index, closingBrace(src, open) + 1);
}
function extractConstObject(name) {
  const re = new RegExp(`\\bconst ${name}\\s*=\\s*\\{`);
  const m = re.exec(src);
  if (!m) throw new Error(`const ${name} not found in app.js`);
  const open = src.indexOf("{", m.index);
  return src.slice(open, closingBrace(src, open) + 1);
}

const fns = [
  "schCondFromValue",
  "schCondBody",
  "schCondError",
  "gateChipText",
  "schBuildTasks",
].map(extractFunction).join("\n");

const sandbox = new Function(
  fns + "\nconst I18N = " + extractConstObject("I18N") + ";\n" +
  "return { schCondFromValue, schCondBody, schCondError, gateChipText, schBuildTasks, I18N };"
)();

/* --- fixtures: conditions exactly as the backend API returns them --- */
const FIXTURES = [
  { cond: null, index: 1, expect: null },
  { cond: { kind: "none" }, index: 1, expect: null }, // backend normalizes to no gate
  { cond: { code: 0, kind: "exit", task_index: 0 }, index: 1 },
  { cond: { code: 7, kind: "exit", task_index: 2 }, index: 3 },
  { cond: { code: -1, kind: "exit", task_index: 0 }, index: 2 },
  { cond: { event: "site.updated", kind: "signal", server_id: 4, timeout_s: 60 }, index: 2 },
  { cond: { event: "backup.complete", kind: "signal", server_id: 9, timeout_s: 3600 }, index: 0 },
];

/* Mirrors the schEdit prepopulate + schSave path in app.js. */
function draftFromCond(cond, serverId) {
  const d = { gate: "none", gateTask: 0, gateCode: 0, gateEvent: "site.updated", gateTimeout: 60, gateServer: serverId, condRaw: null };
  const c = sandbox.schCondFromValue(cond);
  if (c.kind === "exit") { d.gate = "exit"; d.gateTask = c.taskIndex; d.gateCode = c.code; }
  else if (c.kind === "signal") { d.gate = "signal"; d.gateEvent = c.event; d.gateTimeout = c.timeout; d.gateServer = c.serverId; }
  else if (c.kind === "unknown") d.condRaw = c.raw;
  return d;
}

let n = 0;
function check(desc, fn) { fn(); n++; }

for (const fx of FIXTURES) {
  check(`roundtrip ${JSON.stringify(fx.cond)}`, () => {
    const body = sandbox.schCondBody(draftFromCond(fx.cond, 4));
    const out = body === undefined ? null : JSON.stringify(body);
    const want = fx.expect !== undefined ? fx.expect : JSON.stringify(fx.cond);
    assert.strictEqual(out, want, `roundtrip mismatch for ${JSON.stringify(fx.cond)}`);
  });
}

check("unknown kind parsed as unknown + raw preserved", () => {
  const c = sandbox.schCondFromValue({ kind: "future-gate", code: 1 });
  assert.strictEqual(c.kind, "unknown");
  assert.strictEqual(c.raw, '{"kind":"future-gate","code":1}');
});

check("unknown kind survives save byte-identically", () => {
  const body = sandbox.schCondBody(draftFromCond({ kind: "future-gate", code: 1 }, 2));
  assert.strictEqual(JSON.stringify(body), '{"kind":"future-gate","code":1}');
});

check("string-form condition parses", () => {
  const c = sandbox.schCondFromValue('{"code":0,"kind":"exit","task_index":0}');
  assert.strictEqual(c.kind, "exit");
  assert.strictEqual(c.taskIndex, 0);
  assert.strictEqual(c.code, 0);
});

check("fresh exit gate serializes with sorted keys", () => {
  const body = sandbox.schCondBody({ gate: "exit", gateTask: 0, gateCode: 5, gateServer: 2, condRaw: null });
  assert.strictEqual(JSON.stringify(body), '{"code":5,"kind":"exit","task_index":0}');
});

check("fresh signal gate serializes with sorted keys", () => {
  const body = sandbox.schCondBody({ gate: "signal", gateEvent: "deploy.done", gateTimeout: 30, gateServer: 2, condRaw: null });
  assert.strictEqual(JSON.stringify(body), '{"event":"deploy.done","kind":"signal","server_id":2,"timeout_s":30}');
});

check("none omits the condition key", () => {
  assert.strictEqual(sandbox.schCondBody({ gate: "none", condRaw: null }), undefined);
});

/* --- atomic task-batch serialization (shared by create POST + edit PATCH) --- */
check("schBuildTasks emits the full ordered array with canonical conditions", () => {
  const draft = [
    { action: "restart", payload: "", sequence: 1, gate: "none", gateTask: 0, gateCode: 0, gateEvent: "site.updated", gateTimeout: 60, gateServer: 4, condRaw: null },
    { action: "command", payload: "uptime", sequence: 2, gate: "exit", gateTask: 0, gateCode: 0, gateEvent: "site.updated", gateTimeout: 60, gateServer: 4, condRaw: null },
    { action: "notify", payload: "", sequence: 3, gate: "signal", gateEvent: "site.updated", gateTimeout: 60, gateServer: 4, condRaw: null },
    { action: "stop", payload: "", sequence: 4, gate: "none", gateTask: 0, gateCode: 0, gateEvent: "site.updated", gateTimeout: 60, gateServer: 4, condRaw: '{"kind":"future-gate","code":1}' },
  ];
  const tasks = sandbox.schBuildTasks(draft);
  assert.strictEqual(tasks.length, 4);
  assert.deepStrictEqual(tasks[0], { action: "restart", payload: "", sequence: 1 });
  assert.deepStrictEqual(tasks[1], { action: "command", payload: "uptime", sequence: 2, condition: { code: 0, kind: "exit", task_index: 0 } });
  assert.deepStrictEqual(tasks[2], { action: "notify", payload: "", sequence: 3, condition: { event: "site.updated", kind: "signal", server_id: 4, timeout_s: 60 } });
  assert.deepStrictEqual(tasks[3], { action: "stop", payload: "", sequence: 4, condition: { kind: "future-gate", code: 1 } });
  // Wire form of the edit PATCH body: tasks inline, conditions attached,
  // condition key omitted for "no gate", unknown kind re-emitted verbatim.
  const wire = JSON.stringify({ name: "n", cron_expr: "0 0 4 * * *", enabled: true, max_retries: 0, retry_backoff_s: 30, tasks });
  assert.ok(wire.includes('"tasks":'), "edit PATCH body must carry the tasks array");
  assert.ok(wire.includes('"condition":{"code":0,"kind":"exit","task_index":0}'), "exit condition serialized in the task batch");
  assert.ok(wire.includes('"condition":{"event":"site.updated","kind":"signal","server_id":4,"timeout_s":60}'), "signal condition serialized in the task batch");
  assert.ok(!wire.includes('"condition":{"kind":"none"'), "no-gate tasks must omit the condition key");
});

check("schSave edit path is ONE atomic PATCH — delete/re-add loop removed", () => {
  const save = extractFunction("schSave");
  assert.strictEqual((save.match(/method: "PATCH"/g) || []).length, 1, "edit path must issue exactly one PATCH");
  assert.ok(!/DELETE/.test(save), "schSave must not issue any DELETE");
  assert.ok(!/\/tasks\//.test(save), "schSave must not touch the per-task endpoint");
  assert.ok(save.includes("tasks: schBuildTasks(schDraft)"), "PATCH body must carry tasks: schBuildTasks(schDraft)");
});

/* --- validation mirrors backend bounds --- */
const tr = (k) => k;
const E = { gate: "exit", gateTask: 0, gateCode: 0, condRaw: null };
const S = { gate: "signal", gateEvent: "site.updated", gateTimeout: 60, condRaw: null };
check("exit gate cannot reference own/later index", () => {
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 0 }, 0, tr), "gate_err_index");
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 1 }, 1, tr), "gate_err_index");
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 0 }, 1, tr), null);
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 2 }, 3, tr), null);
});
check("exit code must be an integer", () => {
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 0, gateCode: NaN }, 1, tr), "gate_err_code");
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 0, gateCode: 1.5 }, 1, tr), "gate_err_code");
  assert.strictEqual(sandbox.schCondError({ ...E, gateTask: 0, gateCode: -3 }, 1, tr), null);
});
check("signal event must be non-empty", () => {
  assert.strictEqual(sandbox.schCondError({ ...S, gateEvent: "" }, 0, tr), "gate_err_event");
  assert.strictEqual(sandbox.schCondError({ ...S, gateEvent: "  " }, 0, tr), "gate_err_event");
  assert.strictEqual(sandbox.schCondError({ ...S, gateEvent: "x" }, 0, tr), null);
});
check("signal timeout bounds 1..=3600", () => {
  assert.strictEqual(sandbox.schCondError({ ...S, gateTimeout: 0 }, 0, tr), "gate_err_timeout");
  assert.strictEqual(sandbox.schCondError({ ...S, gateTimeout: 3601 }, 0, tr), "gate_err_timeout");
  assert.strictEqual(sandbox.schCondError({ ...S, gateTimeout: NaN }, 0, tr), "gate_err_timeout");
  assert.strictEqual(sandbox.schCondError({ ...S, gateTimeout: 1 }, 0, tr), null);
  assert.strictEqual(sandbox.schCondError({ ...S, gateTimeout: 3600 }, 0, tr), null);
});
check("no gate and preserved unknown pass validation", () => {
  assert.strictEqual(sandbox.schCondError({ gate: "none", condRaw: null }, 0, tr), null);
  assert.strictEqual(sandbox.schCondError({ gate: "none", condRaw: '{"kind":"future"}' }, 0, tr), null);
});

/* --- human-readable chips --- */
const tr2 = (k) => ({
  gate_after_task: "after task {n} exits {code}",
  gate_wait_signal: "wait {event} ≤{s}s",
  gate_unknown: "unknown gate: {raw}",
})[k];
check("chip text for exit gates (1-based task numbers)", () => {
  assert.strictEqual(sandbox.gateChipText({ code: 0, kind: "exit", task_index: 0 }, tr2), "after task 1 exits 0");
  assert.strictEqual(sandbox.gateChipText({ code: 3, kind: "exit", task_index: 2 }, tr2), "after task 3 exits 3");
});
check("chip text for signal gates", () => {
  assert.strictEqual(sandbox.gateChipText({ event: "site.updated", kind: "signal", server_id: 4, timeout_s: 30 }, tr2), "wait site.updated ≤30s");
  assert.strictEqual(sandbox.gateChipText({ event: "backup.complete", kind: "signal", server_id: 4, timeout_s: 3600 }, tr2), "wait backup.complete ≤3600s");
});
check("chip text for none / unknown", () => {
  assert.strictEqual(sandbox.gateChipText(null, tr2), null);
  assert.strictEqual(sandbox.gateChipText({ kind: "none" }, tr2), null);
  assert.strictEqual(sandbox.gateChipText({ kind: "future" }, tr2), 'unknown gate: {"kind":"future"}');
});

/* --- i18n parity: every gate_* key exists in en AND id --- */
const isGateKey = (k) => k === "gate" || k.startsWith("gate_");
const enKeys = Object.keys(sandbox.I18N.en).filter(isGateKey).sort();
const idKeys = Object.keys(sandbox.I18N.id).filter(isGateKey).sort();
check("i18n parity for gate keys (en vs id)", () => {
  assert.deepStrictEqual(enKeys, idKeys, "en/id gate_* key sets differ");
  assert.ok(enKeys.length >= 18, `expected >= 18 gate keys, got ${enKeys.length}`);
});

console.log(`gate-logic: ${n} checks passed (${enKeys.length} gate i18n keys, en/id parity OK)`);
