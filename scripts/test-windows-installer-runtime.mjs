import assert from "node:assert/strict";
import { parseDriverOutput, sleep } from "./windows-installer-runtime.mjs";

const terminated = parseDriverOutput("kill_app", "✅ Terminated pid 7308.");
assert.deepEqual(terminated, {
  status: "terminated",
  pid: 7308,
  raw_output: "✅ Terminated pid 7308.",
});
assert.throws(
  () => parseDriverOutput("kill_app", "❌ Failed to terminate pid 7308."),
  /non-JSON output/,
);
assert.deepEqual(parseDriverOutput("launch_app", '{"pid":7308}'), { pid: 7308 });
assert.throws(() => parseDriverOutput("get_window_state", "Ready"), /non-JSON output/);
assert.ok(sleep(0) instanceof Promise, "sleep returns an awaitable promise");
await sleep(1);
console.log("windows installer runtime helper tests passed (5 assertions)");
