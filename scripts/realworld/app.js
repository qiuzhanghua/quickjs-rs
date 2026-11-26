// A small but complete program in the shape real scripts have: classes with
// inheritance and getters, Map/Set, destructuring, generators, closures, and
// template strings. Every value it prints is checked against the reference
// implementation, so this is a differential test, not a golden-output test.
//
// It exists because three wrong answers in v0.4.0 — the default `Set` iterator
// returning pairs, `Array.from(new Set(...))` returning `[]`, and `break label`
// failing inside `for…of` — were caught by running ordinary business-shaped code,
// not by test262, whose tests are small and single-purpose.

class Shape {
  constructor(name) {
    this.name = name;
  }
  get area() {
    return 0;
  }
  describe() {
    return `${this.name}: ${this.area.toFixed(2)}`;
  }
}

class Rect extends Shape {
  constructor(w, h) {
    super("rect");
    this.w = w;
    this.h = h;
  }
  get area() {
    return this.w * this.h;
  }
}

class Circle extends Shape {
  constructor(r) {
    super("circle");
    this.r = r;
  }
  get area() {
    return Math.PI * this.r * this.r;
  }
}

const shapes = [new Rect(3, 4), new Circle(1)].sort((a, b) => a.area - b.area);
for (const shape of shapes) {
  print(shape.describe());
}

// Map/Set round-trips: the iteration protocol is where the silent bugs lived.
const tally = new Map();
for (const word of ["a", "b", "a", "c", "a"]) {
  tally.set(word, (tally.get(word) || 0) + 1);
}
print([...tally.entries()].map(([k, v]) => `${k}=${v}`).join(","));
print([...new Set([3, 1, 3, 2])].join("-"));
print(JSON.stringify(Array.from(tally.keys()).sort()));

// A generator consumed by a spread and by for-of, plus early break out of both.
function* range(n) {
  for (let i = 0; i < n; i++) yield i;
}
print([...range(4)].join(","));

const collected = [];
const stop = [];
outer: for (const outerValue of range(3)) {
  for (const innerValue of range(3)) {
    if (innerValue > 1) continue outer;
    collected.push(outerValue * 10 + innerValue);
    if (outerValue === 2) break outer;
  }
  stop.push(outerValue);
}
print(collected.join(","));

// Destructuring with defaults, in a closure that captures a loop binding.
const makeAdders = () =>
  [1, 2, 3].map((base) => ({ add: (n = 0) => base + n }));
print(makeAdders().map((f) => f.add(10)).join(","));
const { a = 1, b: renamed = 2, ...rest } = { c: 3, d: 4 };
print(JSON.stringify({ a, renamed, rest }));

// Errors: catch, rethrow through a finally, and read the class off the value.
function risky(n) {
  try {
    if (n < 0) throw new RangeError("negative");
    return n * 2;
  } finally {
    globalThis.finallyRan = (globalThis.finallyRan || 0) + 1;
  }
}
print(risky(2));
try {
  risky(-1);
} catch (error) {
  print(error instanceof RangeError, error.message);
}
try {
  risky(-1);
} catch (error) {
  print(error instanceof RangeError, error.message);
}
print(globalThis.finallyRan);

// Promises and async functions: a chain that resolves in order.
const trace = [];
const chain = async () => {
  trace.push("sync");
  const first = await Promise.resolve(1);
  trace.push(`first=${first}`);
  const all = await Promise.all([first, Promise.resolve(first + 1)]);
  trace.push(`all=${all.join("+")}`);
  try {
    await Promise.reject(new Error("boom"));
  } catch (error) {
    trace.push(`caught=${error.message}`);
  }
  return trace.length;
};
chain().then((length) => print(`${trace.join("|")} -> ${length}`));
print("scheduled");

// String and number formatting, where rounding differences hide.
print((0.1 + 0.2).toFixed(2), (1234.5678).toFixed(2), (255).toString(16));
print(JSON.stringify({ nested: { list: [1, [2, [3]]] } }, null, 0));
print("abc".padStart(6, "-"), "  trim  ".trim(), "a,b,,c".split(",").length);
