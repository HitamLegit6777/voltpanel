"use strict";
/* Smoke test: hostile node-agent payloads must not break out of the crash
   banner's HTML text or value="" attribute (static/js/app.js crashCount).
   No harness exists in this repo, so this is a plain `node` script that
   evaluates app.js in a stubbed browser context and asserts the render.

   Run: node tests/crash-budget.smoke.js
*/
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const src = fs.readFileSync(path.join(__dirname, "..", "static", "js", "app.js"), "utf8");
const noop = () => {};

const sandbox = {
  console,
  window: { addEventListener: noop },
  document: { addEventListener: noop, body: {} },
  MutationObserver: class { constructor() {} observe() {} },
  EventSource: class {},
  location: { hash: "", hostname: "example.test" },
  matchMedia: () => ({ matches: false }),
  navigator: { clipboard: {} },
  setTimeout, clearTimeout, setInterval, clearInterval,
};
vm.createContext(sandbox);

// Assertions run INSIDE the same context so top-level const crashCount/esc are in scope.
const tests = `
(() => {
  const fail = (m) => { throw new Error("FAIL: " + m); };
  const eq = (got, want, m) => { if (got !== want) fail(m + " got " + JSON.stringify(got) + " want " + JSON.stringify(want)); };

  // --- crashCount canonicalization ---
  eq(crashCount('"><img src=x onerror=alert(1)>'), "0", "hostile attr payload -> fallback 0");
  eq(crashCount('1</span><img src=x>'), "1", "digit-prefixed payload collapses to its integer prefix");
  eq(crashCount('12'), "12", "plain integer kept");
  eq(crashCount('7abc'), "7", "numeric prefix canonicalized");
  eq(crashCount('-3'), "0", "negative -> fallback 0");
  eq(crashCount(''), "0", "empty -> fallback 0");
  eq(crashCount(undefined), "0", "missing -> fallback 0");
  eq(crashCount('20'), "20", "max-range value kept");

  // Invariant: whatever the agent sends, crashCount output is digits-only.
  for (const nasty of ['" onfocus="alert(1)', "</span><script>alert(1)</script>", "abc", "1e3", "0x10", null, NaN]) {
    if (!/^\\d+$/.test(crashCount(nasty))) fail("canonicalization broke for " + JSON.stringify(nasty) + " -> " + crashCount(nasty));
  }

  // --- render scenario: attribute breakout ---
  const st = { restart_budget: '" onfocus="alert(1)" x="', restarts_in_burst: '1</span><img src=x onerror=alert(1)>' };
  const inputHtml = '<input id="crash-budget" type="number" min="0" max="20" value="' + esc(crashCount(st.restart_budget)) + '" style="width:56px">';
  if (inputHtml.includes("onfocus")) fail("attribute breakout: input renders attacker attribute");
  if (!/value="\\d+"/.test(inputHtml)) fail("attribute breakout: value is not a bare integer: " + inputHtml);

  const burstHtml = '<span>' + crashCount(st.restarts_in_burst) + ' / ' + crashCount(st.restart_budget) + '</span>';
  if (burstHtml.includes("<img") || burstHtml.includes("</span><")) fail("text breakout: burst line emits markup");
  eq(burstHtml, "<span>1 / 0</span>", "burst line digits-only, no markup");

  console.log("crash-budget smoke OK: canonicalization + no attribute/text breakout");
})();
`;

try {
  vm.runInContext(src + "\n" + tests, sandbox, { filename: "app.js" });
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
