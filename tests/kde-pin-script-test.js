// Execute the production script body against fake KWin windows, never D-Bus.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const source = fs.readFileSync(
  path.join(__dirname, "../crates/vellum-core/src/compositor/kde.rs"),
  "utf8",
);
const match = source.match(/const WINDOW_SCRIPT: &str = r#"([\s\S]*?)"#;/);
assert.ok(match, "production KWin script template must be available");
const pid = 4242;
const title = "vellum 钉图 [vellum-pin:first]";
let cases = 0;

function window(caption = title, extra = {}) {
  return { pid, caption, deleted: false, x: 20, y: 30, ...extra };
}

function execute(windows, args) {
  const script = `(${match[1]}).apply(null, ${JSON.stringify(args)});`;
  vm.runInNewContext(
    script,
    { workspace: { windowList: () => windows } },
    { timeout: 1000 },
  );
  cases++;
}

function apply(windows, target = title, resize = true) {
  execute(windows, [
    pid,
    target,
    null,
    resize ? 320 : 0,
    resize ? 180 : 0,
    !resize,
  ]);
}

for (const reversed of [false, true]) {
  const pin = window(title, { stackingOrder: -100 });
  const viewer = window("viewer", { stackingOrder: 100 });
  const otherPin = window("vellum 钉图 [vellum-pin:second]", {
    stackingOrder: 200,
  });
  const foreign = window(title, { pid: 9999, stackingOrder: 300 });
  const untouched = JSON.stringify([viewer, otherPin, foreign]);
  const windows = [viewer, pin, otherPin, foreign];
  if (reversed) windows.reverse();
  apply(windows, title, false);
  assert.equal(pin.keepAbove, true);
  assert.equal(pin.skipTaskbar, true);
  assert.equal(pin.frameGeometry, undefined);
  // Raising another pin after the float must not change the saved target.
  otherPin.stackingOrder = 1000;
  const beforeResize = JSON.stringify([viewer, otherPin, foreign]);
  apply(windows);
  assert.deepEqual(JSON.parse(JSON.stringify(pin.frameGeometry)), {
    x: 20,
    y: 30,
    width: 320,
    height: 180,
  });
  assert.equal(JSON.stringify([viewer, otherPin, foreign]), beforeResize);
  otherPin.stackingOrder = 200;
  assert.equal(JSON.stringify([viewer, otherPin, foreign]), untouched);
}

for (const windows of [
  [],
  [window("viewer")],
  [window("changed title")],
  [window(title, { deleted: true }), window("viewer")],
  [window(title, { pid: 9999 }), window("viewer")],
  [window(), window()],
]) {
  const before = JSON.stringify(windows);
  apply(windows, title, false);
  apply(windows);
  assert.equal(
    JSON.stringify(windows),
    before,
    "miss or ambiguity must be a no-op",
  );
}

// A duplicate-title suffix must not cause the unsuffixed sibling to be chosen.
for (const suffix of [" <2>", " <2>\u200e", " (2)"]) {
  for (const reversed of [false, true]) {
    const suffixed = window(title + suffix);
    const unsuffixed = window();
    const windows = [suffixed, unsuffixed, window("viewer")];
    if (reversed) windows.reverse();
    const before = JSON.stringify(windows);
    apply(windows, title, false);
    apply(windows);
    assert.equal(
      JSON.stringify(windows),
      before,
      "duplicate caption suffix must veto the operation",
    );
  }
  const windows = [window(title + suffix), window("viewer")];
  const before = JSON.stringify(windows);
  apply(windows);
  assert.equal(
    JSON.stringify(windows),
    before,
    "a suffixed caption alone is not an exact target",
  );
}

// Only live windows of our pid can create prefix ambiguity.
const exact = window();
const ignoredSuffixes = [
  window(title + " <2>", { deleted: true }),
  window(title + " <3>", { pid: 9999 }),
];
apply([exact, ...ignoredSuffixes]);
assert.equal(exact.frameGeometry.width, 320);
assert.ok(ignoredSuffixes.every((w) => w.frameGeometry === undefined));

// The legacy placement path has no title; ambiguity must not select topmost.
const ambiguous = [window("viewer"), window("panel")];
const before = JSON.stringify(ambiguous);
apply(ambiguous, null);
assert.equal(JSON.stringify(ambiguous), before);

const panel = window("panel");
execute(
  [panel, window("foreign panel", { pid: 9999 })],
  [pid, null, [-300, 40], 200, 100, true],
);
assert.deepEqual(JSON.parse(JSON.stringify(panel.frameGeometry)), {
  x: -300,
  y: 40,
  width: 200,
  height: 100,
});
assert.equal(panel.keepAbove, true);

const live = window();
const deleted = window(title, { deleted: true });
apply([deleted, live]);
assert.equal(live.frameGeometry.width, 320);
assert.equal(deleted.frameGeometry, undefined);

for (const [width, height] of [
  [0, 100],
  [100, 0],
  [-1, 100],
  [100, -1],
]) {
  const pin = window();
  const before = JSON.stringify(pin);
  execute([pin], [pid, title, null, width, height, false]);
  assert.equal(JSON.stringify(pin), before);
}

const hostileTitle =
  '钉图 "\\\n\'); throw new Error("injected"); //\u2028\u2029';
const hostile = window(hostileTitle);
apply([hostile, window("viewer")], hostileTitle);
assert.equal(hostile.frameGeometry.width, 320);
console.log(`KWin script fixture: ${cases} cases passed (no desktop calls)`);
