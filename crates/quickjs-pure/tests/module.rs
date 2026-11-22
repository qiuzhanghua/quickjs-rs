//! Modules: linking, cycles, namespaces, and data modules.
//!
//! The tests use an in-memory loader rather than the filesystem, which is the point of
//! `ModuleLoader` being a trait — the semantics under test are the graph's, not the
//! disk's. The two properties worth stating up front, because they are what most module
//! implementations get wrong:
//!
//! * **Live bindings.** `export let x` and a later `x = 1` is visible to the importer.
//!   Copying the value at link time passes every simple test and fails this one.
//! * **Cycles.** Two modules that import each other must link, hoist, and evaluate in
//!   the right order, so that a function in one is callable from the other's body.

use std::collections::BTreeMap;

use quickjs::module::{LoadedModule, ModuleLoader};

/// A loader over a fixed set of sources, keyed by the name a specifier resolves to.
struct MemoryLoader {
    sources: BTreeMap<String, Vec<u8>>,
}

impl MemoryLoader {
    /// Resolving `./x.js` against `/m/main.js` gives `/m/x.js`, exactly as a file loader
    /// would, without touching a disk. `.` and `..` are folded because a real loader
    /// canonicalises, and a specifier that only differs by a dot must be one module.
    fn resolve(specifier: &str, referrer: &str) -> String {
        if referrer.is_empty() {
            return specifier.to_string();
        }
        let dir = match referrer.rfind('/') {
            Some(index) => &referrer[..index],
            None => "",
        };
        let joined = format!("{dir}/{specifier}");
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        format!("/{}", parts.join("/"))
    }
}

impl ModuleLoader for MemoryLoader {
    fn load(
        &self,
        specifier: &str,
        _attributes: &[(String, String)],
        referrer: &str,
    ) -> Result<LoadedModule, String> {
        let name = Self::resolve(specifier, referrer);
        let source = self
            .sources
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("could not load module filename '{specifier}'"))?;
        Ok(LoadedModule {
            url: format!("file://{name}"),
            name,
            source,
        })
    }
}

/// Run `/m/main.js` in a world where `files` are the only sources that exist.
fn run(files: &[(&str, &str)]) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let sources = files
        .iter()
        .map(|(name, source)| (format!("/m/{name}"), source.as_bytes().to_vec()))
        .collect();
    ctx.set_module_loader(Box::new(MemoryLoader { sources }));
    match ctx.eval_module("/m/main.js") {
        Ok(_) => ctx.output().to_string(),
        Err(error) => panic!(
            "{files:?} failed: {error}\noutput so far:\n{}",
            ctx.output()
        ),
    }
}

/// Like [`run`], but the failure is what is being tested.
fn fails(files: &[(&str, &str)]) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let sources = files
        .iter()
        .map(|(name, source)| (format!("/m/{name}"), source.as_bytes().to_vec()))
        .collect();
    ctx.set_module_loader(Box::new(MemoryLoader { sources }));
    match ctx.eval_module("/m/main.js") {
        Ok(_) => panic!("{files:?} should have failed"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_imported_binding_is_the_exporters_cell_not_a_copy() {
    // `bump()` runs in `lib.js` and assigns `counter` there; the importer must see it.
    // Copying at link time would leave `ns.counter` at 0 forever.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import def, * as ns from "./lib.js";
print("default:" + def);
print("keys:" + Object.keys(ns).join(","));
print("before:" + ns.counter);
ns.bump();
print("after:" + ns.counter);
"#
            ),
            (
                "lib.js",
                r#"
export let counter = 0;
export function bump() { counter++ }
export default "D";
"#
            ),
        ]),
        "default:D\nkeys:bump,counter,default\nbefore:0\nafter:1\n"
    );
}

#[test]
fn a_cycle_hoists_before_any_body_runs() {
    // `b.js` calls `a`'s function from its own top level. That only works because every
    // module in the cycle has had its hoisted bindings created during linking — before a
    // single body has run. Getting the order wrong here is the classic failure.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import { a } from "./a.js";
print("main:" + a(1));
"#
            ),
            (
                "a.js",
                r#"
import { b } from "./b.js";
export function a(x) { return x + 1 }
print("a sees b: " + typeof b);
"#
            ),
            (
                "b.js",
                r#"
import { a } from "./a.js";
export function b(x) { return 2 * a(x) }
print("b sees a: " + typeof a);
print("b calls a: " + a(1));
"#
            ),
        ]),
        // `b` evaluates first (it is the innermost dependency), and `a`'s function is
        // already there because linking hoisted both modules.
        "b sees a: function\nb calls a: 2\na sees b: function\nmain:2\n"
    );
}

#[test]
fn a_namespace_is_sorted_frozen_and_tagged() {
    // §10.4.6: sorted keys, not extensible, `[object Module]`. The getters make the
    // bindings live; the sort is what makes `Object.keys` deterministic.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import * as ns from "./lib.js";
print(Object.keys(ns).join(","));
print(Object.prototype.toString.call(ns));
print(Object.isExtensible(ns));
try { ns.extra = 1 } catch (e) { print("assign threw") }
print("extra" in ns);
"#
            ),
            (
                "lib.js",
                r#"
export const zebra = 1;
export const alpha = 2;
export function middle() {}
"#
            ),
        ]),
        "alpha,middle,zebra\n[object Module]\nfalse\nassign threw\nfalse\n"
    );
}

#[test]
fn re_exports_forward_names_and_their_own_default() {
    // `export *` forwards everything except `default`, while `export * as ns` and
    // `export { x } from` name things explicitly.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import * as barrel from "./barrel.js";
print("barrel keys:" + Object.keys(barrel).join(","));
print("a:" + barrel.a);
print("aliased:" + barrel.renamed);
print("ns:" + Object.keys(barrel.ns).join(","));
"#
            ),
            (
                "barrel.js",
                r#"
export * from "./base.js";
export { b as renamed } from "./base.js";
export * as ns from "./base.js";
"#
            ),
            (
                "base.js",
                r#"
export const a = "A";
export const b = "B";
export default "default-not-forwarded";
"#
            ),
        ]),
        // `b` appears twice over: once forwarded by `export *` under its own name and
        // once as `renamed`. `default` is absent, because `export *` never forwards it.
        "barrel keys:a,b,ns,renamed\na:A\naliased:B\nns:a,b,default\n"
    );
}

#[test]
fn json_and_text_modules_are_two_modules_from_one_file() {
    // The same path as JSON and as text: two records, two values. Keying the cache by
    // path alone would make the second import silently receive the first one's value.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import data from "./data.json";
import text from "./data.json" with { type: "text" };
print(JSON.stringify(data));
print(typeof text);
print(JSON.stringify(text));
"#
            ),
            ("data.json", "{\"n\": [1, 2]}"),
        ]),
        "{\"n\":[1,2]}\nstring\n\"{\\\"n\\\": [1, 2]}\"\n"
    );
}

#[test]
fn a_dynamic_import_settles_after_the_current_script() {
    // §13.3.10.1 makes `import()` settle through a job, so the synchronous tail runs
    // first — the same ordering rule a promise follows.
    assert_eq!(
        run(&[
            (
                "main.js",
                r#"
import("./lib.js").then(m => print("dynamic:" + m.value));
print("sync");
"#
            ),
            ("lib.js", "export const value = 42;\n"),
        ]),
        "sync\ndynamic:42\n"
    );
}

#[test]
fn a_missing_export_is_reported_at_link_time() {
    // `export { nope } from "m"` where `m` has no `nope`: the graph cannot be linked, and
    // saying so beats an `undefined` that surfaces at use.
    let text = fails(&[
        (
            "main.js",
            "import { nope } from \"./lib.js\";\nprint(nope);\n",
        ),
        ("lib.js", "export const yes = 1;\n"),
    ]);
    // The reference's wording, because an error message is behaviour.
    assert!(
        text.contains("Could not find export 'nope' in module"),
        "unexpected error: {text}"
    );
}

#[test]
fn a_module_has_no_arguments_and_no_global_leak() {
    // Module scope is not global scope: `var` here must not reach the global object, and
    // `arguments` must not exist at all.
    assert_eq!(
        run(&[(
            "main.js",
            r#"
var leaked = 1;
function f(){ return leaked }
print(f());
print(typeof arguments);
print(typeof globalThis.leaked);
"#
        ),]),
        "1\nundefined\nundefined\n"
    );
}

#[test]
fn import_meta_names_the_module_and_marks_the_entry() {
    assert_eq!(
        run(&[
            ("main.js", "print(import.meta.url); import \"./lib.js\";\n"),
            (
                "lib.js",
                "print(import.meta.url); print(import.meta.main);\n"
            ),
        ]),
        "file:///m/lib.js\nfalse\nfile:///m/main.js\n"
    );
}
