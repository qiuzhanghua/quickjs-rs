// module: yes
// A module-shaped program: ES module syntax, top-level await, and JSON import.
//
// This one exists because the module system's tangle (link-time body execution for
// hoisted bindings, per-attribute module cache keys, live bindings) was the source of
// three silently wrong answers in v0.4.0. The `data` import is the same file twice
// under two attributes on purpose: the cache key must include attributes, or the
// second import silently gets the first one's value.

import data from "./data.json" with { type: "json" };

const doubled = data.values.map((value) => value * 2);
print(JSON.stringify(doubled));

// Top-level await: the module body has to run as a coroutine.
const resolved = await Promise.resolve(doubled.reduce((sum, value) => sum + value, 0));
print(`sum=${resolved}`);

// A live binding observed through a getter, after mutation from a second statement.
let counter = 0;
const read = () => counter;
counter += 1;
print(read(), counter);

// export is exercised by the harness invoking the module; the value must survive
// the module body running twice (once to hoist, once to evaluate).
export const answer = resolved;
export function describe() {
  return `data:${data.values.length},sum:${resolved}`;
}
print(describe());
