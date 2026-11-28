//! The module system: records, linking, namespaces, and evaluation.
//!
//! The design follows §16.2 closely but leans on one thing this engine already has.
//! A module's top-level bindings compile as *closure variables* of the module body —
//! `ClosureType::ModuleDecl` for its own declarations, `ClosureType::ModuleImport` for
//! what it imports — so reading one is an ordinary `get_var_ref`. What makes modules
//! interesting is then entirely a question of **who fills the cells**:
//!
//! * a declaration gets a fresh cell, created here;
//! * an import gets *the exporting module's* cell, not a copy.
//!
//! Sharing the cell is what makes bindings live (`export let x; x = 1` is visible to the
//! importer) and what makes cyclic imports work: linking wires cells before anything
//! evaluates, so `a.js` importing from a `b.js` that imports back from `a.js` resolves to
//! cells on both sides without either body having run.
//!
//! The three phases are the spec's: load (read and compile), link (resolve imports,
//! validate exports), evaluate (run bodies depth-first, once each).

use std::collections::BTreeMap;

use js_gc::{Gc, Trace, Tracer};

use crate::interp::{Interp, JsResult, Signal, WellKnown};
use crate::object::{Callable, ClosureEnv, CompiledCode, JsObject, Property};
use crate::value::{Cell, Value};

/// Where a module is in its lifetime.
///
/// `Linking` and `Evaluated` are not just bookkeeping: they are what makes a cycle
/// terminate. A cycle is not an error — it is a graph that must be *linked* depth-first
/// and then *evaluated* in post-order, and the two-phase split is the mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleStatus {
    /// Loaded and compiled; imports not yet resolved.
    Unlinked,
    /// Being linked, so a re-entrant link must not recurse.
    Linking,
    /// Imports resolved and exports validated.
    Linked,
    /// Body running or already run, so a re-entrant evaluation must not run it twice.
    Evaluated,
}

/// How a module's source became a module.
///
/// A JSON or text module is not code at all — it is one value exported as `default` — so
/// it has no body and no imports, which is why this is a flag rather than a second type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    /// JavaScript: has a compiled body.
    JavaScript,
    /// A parsed JSON document, exported as `default`.
    Json,
    /// A plain string, exported as `default`.
    Text,
    /// A `Uint8Array`, exported as `default`.
    Bytes,
}

/// One loaded module.
///
/// Held on the GC heap because its cells and namespace are GC values. The whole record is
/// behind interior mutability the same way a [`Cell`] is: linking writes into it while
/// other modules already hold references to it.
pub struct ModuleRecord {
    inner: std::cell::RefCell<ModuleInner>,
}

impl ModuleRecord {
    /// Build a record around `inner`.
    pub fn new(inner: ModuleInner) -> ModuleRecord {
        ModuleRecord {
            inner: std::cell::RefCell::new(inner),
        }
    }

    /// Run `f` with the record borrowed.
    pub fn with<R>(&self, f: impl FnOnce(&ModuleInner) -> R) -> R {
        f(&self.inner.borrow())
    }

    /// Run `f` with the record borrowed mutably.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut ModuleInner) -> R) -> R {
        f(&mut self.inner.borrow_mut())
    }
}

/// Everything a module record holds; see [`ModuleRecord`].
pub struct ModuleInner {
    /// Canonical identity: the cache key, and what a cycle compares.
    pub name: String,
    /// What `import.meta.url` reports.
    pub url: String,
    /// The compiled body, or `None` for a JSON/text/bytes module.
    pub code: Option<Gc<CompiledCode>>,
    /// The tables the compiler produced.
    pub meta: js_compile::bytecode::ModuleMeta,
    /// One cell per closure variable of the body; the body reads its bindings here.
    pub cells: Vec<Gc<Cell>>,
    /// Resolved dependencies, parallel to `meta.requests`.
    pub dependencies: Vec<Option<Gc<ModuleRecord>>>,
    /// Where it is in its lifetime.
    pub status: ModuleStatus,
    /// What kind of module it is.
    pub kind: ModuleKind,
    /// The namespace object, built on first use.
    pub namespace: Option<Gc<JsObject>>,
    /// `import.meta`, built at load time.
    pub import_meta: Option<Gc<JsObject>>,
}

impl Trace for ModuleRecord {
    fn trace(&self, tracer: &mut Tracer) {
        self.inner.borrow().trace(tracer);
    }
}

impl Trace for ModuleInner {
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(code) = &self.code {
            tracer.visit(code);
        }
        for cell in &self.cells {
            tracer.visit(cell);
        }
        for dependency in self.dependencies.iter().flatten() {
            tracer.visit(dependency);
        }
        if let Some(namespace) = &self.namespace {
            tracer.visit(namespace);
        }
        if let Some(meta) = &self.import_meta {
            tracer.visit(meta);
        }
    }
}

/// A host-supplied module, before it is compiled.
pub struct LoadedModule {
    /// Canonical identity — the cache key, and how a cycle recognises itself.
    pub name: String,
    /// `import.meta.url` for this module.
    pub url: String,
    /// The file's bytes. Not necessarily UTF-8: a `bytes` module is arbitrary.
    pub source: Vec<u8>,
}

/// How the host turns a specifier into bytes.
///
/// The engine cannot do this itself: resolving `./x.js` needs to know that the importer
/// was a file, and where. A browser-shaped host would answer differently, so the host
/// supplies this and the engine decides what to do with the bytes.
pub trait ModuleLoader {
    /// Resolve `specifier` relative to `referrer` and read it.
    ///
    /// `referrer` is empty for the entry module.
    fn load(
        &self,
        specifier: &str,
        attributes: &[(String, String)],
        referrer: &str,
    ) -> Result<LoadedModule, String>;
}

impl<'a> Interp<'a> {
    /// The module registry: one entry per canonical name, so each file is compiled once.
    pub fn module_registry(&self) -> &BTreeMap<String, Gc<ModuleRecord>> {
        &self.modules
    }

    /// Evaluate `specifier` as the entry module, the way `qjs -m file.js` does.
    pub fn run_main_module(&mut self, specifier: &str) -> JsResult<Value> {
        let module = self.load_module(specifier, &[], "")?;
        let name = module.with(|r| r.name.clone());
        self.main_module_name = Some(name.clone());
        // The record was built before the name was known, so `main` is stamped now.
        if let Some(meta) = module.with(|r| r.import_meta.clone()) {
            meta.define_data("main", Value::Bool(true));
        }
        self.link_module(&module)?;
        self.module_stack.push(name);
        let result = self.evaluate_module_body(&module);
        self.module_stack.pop();
        result?;
        self.module_namespace(&module)
    }

    /// Load and compile one module.
    pub fn load_module(
        &mut self,
        specifier: &str,
        attributes: &[(String, String)],
        referrer: &str,
    ) -> JsResult<Gc<ModuleRecord>> {
        let Some(loader) = self.module_loader.take() else {
            return Err(Signal::Throw(
                self.make_error("InternalError", "no module loader is installed"),
            ));
        };
        let loaded = loader.load(specifier, attributes, referrer);
        // Put the loader back whatever happened: it is the host's, and the interpreter
        // only borrows it so that `&mut self` stays available for compiling.
        self.module_loader = Some(loader);
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(message) => {
                return Err(Signal::Throw(self.make_error("ReferenceError", &message)));
            }
        };
        // One record per *(name, attributes)*, so importing the same file twice runs it
        // once — but importing it as JSON and as text is two modules with two contents,
        // which `hello_module.js` does on purpose.
        let key = cache_key(&loaded.name, attributes);
        if let Some(existing) = self.modules.get(&key) {
            return Ok(existing.clone());
        }
        let kind = match module_kind(&loaded.name, attributes) {
            Ok(kind) => kind,
            Err(message) => {
                return Err(Signal::Throw(self.make_error("TypeError", &message)));
            }
        };
        let record = match kind {
            ModuleKind::JavaScript => self.compile_module_record(&loaded)?,
            _ => self.data_module_record(&loaded, kind)?,
        };
        self.modules.insert(key, record.clone());
        Ok(record)
    }

    /// Compile a JavaScript module and create its cells.
    fn compile_module_record(&mut self, loaded: &LoadedModule) -> JsResult<Gc<ModuleRecord>> {
        let source = match std::str::from_utf8(&loaded.source) {
            Ok(source) => source,
            Err(_) => {
                return Err(Signal::Throw(self.make_error(
                    "SyntaxError",
                    &format!("{}: module source is not valid UTF-8", loaded.name),
                )));
            }
        };
        // Same shape as a script's compile error: the message, then `at <file>:<line>:<col>`
        // unless the reference reports this one without a position. The file name is *not*
        // part of the message — it belongs on the `at` line only.
        let program = match js_parse::parse_module(source) {
            Ok(program) => program,
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_plain(&error, &loaded.name)));
            }
        };
        let mut code = match js_compile::compile_module(&program) {
            Ok(code) => code,
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_plain(&error, &loaded.name)));
            }
        };
        code.set_filename(&loaded.name);
        let meta = code
            .module
            .as_ref()
            .map(|meta| (**meta).clone())
            .unwrap_or_default();
        let closure_vars = code.closure_vars.clone();
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        // One cell per closure variable. A lexical binding starts in its dead zone and a
        // `var` as `undefined`; an import's cell is *replaced* during linking by the
        // exporter's, which is what makes it live.
        let mut cells = Vec::with_capacity(closure_vars.len());
        for closure_var in &closure_vars {
            let initial = if closure_var.is_lexical {
                Value::Uninitialized
            } else {
                Value::Undefined
            };
            cells.push(self.new_cell(initial));
        }
        let dependencies = vec![None; meta.requests.len()];
        let record = Gc::new(
            self.heap,
            ModuleRecord::new(ModuleInner {
                name: loaded.name.clone(),
                url: loaded.url.clone(),
                code: Some(compiled),
                meta,
                cells,
                dependencies,
                status: ModuleStatus::Unlinked,
                kind: ModuleKind::JavaScript,
                namespace: None,
                import_meta: None,
            }),
        );
        self.install_import_meta(&record)?;
        Ok(record)
    }

    /// A JSON, text or bytes module: one value, exported as `default`.
    ///
    /// The value is produced by calling the *builtin* rather than by reaching into the
    /// JSON parser or the typed-array internals, so a data module behaves exactly like
    /// `JSON.parse(text)` or `new Uint8Array(bytes)` written out by hand — one
    /// implementation of each, not two.
    fn data_module_record(
        &mut self,
        loaded: &LoadedModule,
        kind: ModuleKind,
    ) -> JsResult<Gc<ModuleRecord>> {
        let value = match kind {
            ModuleKind::Json => {
                let text = String::from_utf8_lossy(&loaded.source).to_string();
                let Some(json) = self.global_value("JSON") else {
                    return Err(Signal::Throw(
                        self.make_error("InternalError", "JSON is not installed"),
                    ));
                };
                let Some(Value::Object(json)) = Some(json) else {
                    unreachable!()
                };
                let Some(parse) = self.get_property(&json, "parse")? else {
                    return Err(Signal::Throw(
                        self.make_error("InternalError", "JSON.parse is not installed"),
                    ));
                };
                let text = self.string(&text);
                match self.call_value(parse, Value::Object(json), vec![text]) {
                    Ok(value) => value,
                    Err(Signal::Throw(reason)) => {
                        let detail = self.describe_exception(&reason);
                        return Err(Signal::Throw(self.make_error(
                            "SyntaxError",
                            &format!("{}: {}", loaded.name, detail),
                        )));
                    }
                    Err(other) => return Err(other),
                }
            }
            ModuleKind::Text => {
                // Taken as UTF-8 with replacement: a text file that is not valid UTF-8
                // still has to become a string.
                let text = String::from_utf8_lossy(&loaded.source).to_string();
                self.string(&text)
            }
            ModuleKind::Bytes => {
                let numbers: Vec<Value> = loaded
                    .source
                    .iter()
                    .map(|byte| Value::Int(*byte as i32))
                    .collect();
                let elements = self.new_array(numbers);
                let Some(Value::Object(ctor)) = self.global_value("Uint8Array") else {
                    return Err(Signal::Throw(
                        self.make_error("InternalError", "Uint8Array is not installed"),
                    ));
                };
                self.construct(
                    Value::Object(ctor),
                    Value::Object(elements.clone()),
                    vec![Value::Object(elements)],
                )?
            }
            ModuleKind::JavaScript => unreachable!("handled by `compile_module_record`"),
        };
        let cell = self.new_cell(value.clone());
        let record = Gc::new(
            self.heap,
            ModuleRecord::new(ModuleInner {
                name: loaded.name.clone(),
                url: loaded.url.clone(),
                code: None,
                meta: js_compile::bytecode::ModuleMeta {
                    requests: Vec::new(),
                    imports: Vec::new(),
                    exports: vec![js_compile::bytecode::ExportEntry::Local {
                        export_name: "default".to_string(),
                        var_idx: 0,
                    }],
                },
                cells: vec![cell],
                dependencies: Vec::new(),
                // Nothing to link and nothing to run: a data module is born evaluated,
                // which is what lets `import x from "./m.json"` work inside a cycle.
                status: ModuleStatus::Evaluated,
                kind,
                namespace: None,
                import_meta: None,
            }),
        );
        Ok(record)
    }

    /// A binding of the global object, if it exists.
    fn global_value(&mut self, name: &str) -> Option<Value> {
        let global = self.realm.global.clone();
        self.get_property(&global, name).ok().flatten()
    }

    /// `import.meta` for `record`, built once.
    fn install_import_meta(&mut self, record: &Gc<ModuleRecord>) -> JsResult<()> {
        let proto = self.realm.object_proto.clone();
        let meta = self.new_object(Some(proto));
        let url = self.string(record.with(|r| r.url.clone()));
        // Only the two properties the reference defines; a script can add its own.
        meta.define_data("url", url);
        meta.define_data("main", Value::Bool(false));
        record.with_mut(|r| r.import_meta = Some(meta));
        Ok(())
    }

    /// Resolve every import of `module` and validate its exports, depth-first.
    pub fn link_module(&mut self, module: &Gc<ModuleRecord>) -> JsResult<()> {
        match module.with(|r| r.status) {
            // Already linked, or being linked right now — the latter is a cycle, and it is
            // exactly why the status is set *before* recursing: a cycle stops here rather
            // than recursing forever.
            ModuleStatus::Linking | ModuleStatus::Linked | ModuleStatus::Evaluated => {
                return Ok(());
            }
            ModuleStatus::Unlinked => {}
        }
        module.with_mut(|r| r.status = ModuleStatus::Linking);

        let requests = module.with(|r| r.meta.requests.clone());
        for (index, request) in requests.iter().enumerate() {
            let referrer = module.with(|r| r.name.clone());
            let dependency =
                self.load_module(&request.specifier, &request.attributes, &referrer)?;
            module.with_mut(|r| r.dependencies[index] = Some(dependency.clone()));
            self.link_module(&dependency)?;
        }

        // Wire the cells. `import { x } from "m"` makes this module's cell *be* `m`'s, so
        // a later assignment over there is visible here with no copying.
        let imports = module.with(|r| r.meta.imports.clone());
        for entry in &imports {
            let dependency =
                module.with(|r| r.dependencies.get(entry.module as usize).cloned().flatten());
            let Some(dependency) = dependency else {
                return Err(Signal::Throw(self.make_error(
                    "InternalError",
                    "an import names a module that was not loaded",
                )));
            };
            if entry.is_star {
                let namespace = self.module_namespace(&dependency)?;
                let cell = module.with(|r| r.cells[entry.var_idx as usize].clone());
                cell.set(namespace);
                continue;
            }
            let cell = self.resolve_export(&dependency, &entry.name)?;
            module.with_mut(|r| r.cells[entry.var_idx as usize] = cell);
        }

        // Validate: `export { a } from "m"` where `m` has no `a` is a link error, and
        // saying so now beats an `undefined` that surfaces much later.
        let exports = module.with(|r| r.meta.exports.clone());
        for entry in &exports {
            if let js_compile::bytecode::ExportEntry::Indirect {
                export_name,
                module: index,
                import_name,
            } = entry
            {
                let dependency =
                    module.with(|r| r.dependencies.get(*index as usize).cloned().flatten());
                let Some(dependency) = dependency else {
                    continue;
                };
                if self.resolve_export(&dependency, import_name).is_err() {
                    let dependency_name = dependency.with(|r| r.name.clone());
                    return Err(Signal::Throw(self.make_error(
                        "SyntaxError",
                        &format!(
                            "Could not find export '{import_name}' in module '{dependency_name}' (re-exported as '{export_name}')"
                        ),
                    )));
                }
            }
        }

        // Create the module's hoisted bindings *now*, while linking — before any body
        // runs anywhere in the graph.
        //
        // This is the whole trick behind cyclic imports. The body's prologue is
        // `push_this; if_false <body>`, so calling it with a truthy `this` runs only the
        // hoisted declarations and returns. Doing that for every module during linking
        // means that when evaluation finally starts, every exported function in the
        // cycle already exists — which is why `b.js` can see `a`'s function even though
        // `a.js` has not run yet.
        self.hoist_module_bindings(module)?;

        module.with_mut(|r| r.status = ModuleStatus::Linked);
        Ok(())
    }

    /// Run `module`'s prologue only, creating its hoisted bindings.
    fn hoist_module_bindings(&mut self, module: &Gc<ModuleRecord>) -> JsResult<()> {
        let Some(code) = module.with(|r| r.code.clone()) else {
            // A JSON/text module has nothing to hoist.
            return Ok(());
        };
        let env = self.module_env(module);
        self.execute(code, env, Value::Bool(true), Vec::new(), Value::Undefined)
            .map(|_| ())
    }

    /// The closure environment a module body runs in: one cell per closure variable.
    fn module_env(&mut self, module: &Gc<ModuleRecord>) -> Gc<ClosureEnv> {
        let cells = module.with(|r| r.cells.clone());
        Gc::new(
            self.heap,
            ClosureEnv {
                cells,
                eval_scope: None,
                local_vars: None,
                local_phase: 0,
            },
        )
    }

    /// The cell behind `name` in `module`, following re-exports.
    pub fn resolve_export(&mut self, module: &Gc<ModuleRecord>, name: &str) -> JsResult<Gc<Cell>> {
        self.resolve_export_inner(module, name, 0)
    }

    fn resolve_export_inner(
        &mut self,
        module: &Gc<ModuleRecord>,
        name: &str,
        depth: usize,
    ) -> JsResult<Gc<Cell>> {
        // A pathological chain (`a` exports from `b`, `b` from `a`) would otherwise
        // recurse until the Rust stack gives out.
        if depth > 64 {
            return Err(Signal::Throw(
                self.make_error("SyntaxError", "circular re-export chain"),
            ));
        }
        let exports = module.with(|r| r.meta.exports.clone());
        // An explicit export wins over any `export *`.
        for entry in &exports {
            match entry {
                js_compile::bytecode::ExportEntry::Local {
                    export_name,
                    var_idx,
                } if export_name == name => {
                    return Ok(module.with(|r| r.cells[*var_idx as usize].clone()));
                }
                js_compile::bytecode::ExportEntry::Indirect {
                    export_name,
                    module: index,
                    import_name,
                } if export_name == name => {
                    let dependency =
                        module.with(|r| r.dependencies.get(*index as usize).cloned().flatten());
                    let Some(dependency) = dependency else {
                        return Err(self.missing_export(module, name));
                    };
                    return self.resolve_export_inner(&dependency, import_name, depth + 1);
                }
                js_compile::bytecode::ExportEntry::StarAs { export_name, .. }
                    if export_name == name =>
                {
                    // The namespace of the re-exported module, wrapped in a cell so the
                    // export table stays uniform. A fresh cell each time is harmless:
                    // nothing can assign through it.
                    let namespace = self.star_namespace(module, name)?;
                    return Ok(self.new_cell(namespace));
                }
                _ => {}
            }
        }
        // `export * from "m"`: forwarded names are visible only if nothing above claimed
        // them, which is why this is a second pass.
        for entry in &exports {
            if let js_compile::bytecode::ExportEntry::Star { module: index } = entry {
                let dependency =
                    module.with(|r| r.dependencies.get(*index as usize).cloned().flatten());
                let Some(dependency) = dependency else {
                    continue;
                };
                if let Ok(cell) = self.resolve_export_inner(&dependency, name, depth + 1) {
                    return Ok(cell);
                }
            }
        }
        Err(self.missing_export(module, name))
    }

    fn missing_export(&self, module: &Gc<ModuleRecord>, name: &str) -> Signal {
        let module_name = module.with(|r| r.name.clone());
        // The reference's wording, kept verbatim: an error message is part of the
        // behaviour a user sees, and `qjs` says "Could not find export".
        Signal::Throw(self.make_error(
            "SyntaxError",
            &format!("Could not find export '{name}' in module '{module_name}'"),
        ))
    }

    /// The namespace behind `export * as ns from "m"`.
    fn star_namespace(&mut self, module: &Gc<ModuleRecord>, name: &str) -> JsResult<Value> {
        let exports = module.with(|r| r.meta.exports.clone());
        for entry in &exports {
            if let js_compile::bytecode::ExportEntry::StarAs {
                export_name,
                module: index,
            } = entry
                && export_name == name
            {
                let dependency =
                    module.with(|r| r.dependencies.get(*index as usize).cloned().flatten());
                let Some(dependency) = dependency else {
                    continue;
                };
                return self.module_namespace(&dependency);
            }
        }
        Ok(Value::Undefined)
    }

    /// The names `module` exports, sorted the way §16.2.1.6 requires.
    pub fn module_export_names(&mut self, module: &Gc<ModuleRecord>) -> JsResult<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        self.collect_export_names(module, &mut names, 0)?;
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn collect_export_names(
        &mut self,
        module: &Gc<ModuleRecord>,
        out: &mut Vec<String>,
        depth: usize,
    ) -> JsResult<()> {
        if depth > 64 {
            return Ok(());
        }
        let exports = module.with(|r| r.meta.exports.clone());
        for entry in &exports {
            match entry {
                js_compile::bytecode::ExportEntry::Local { export_name, .. }
                | js_compile::bytecode::ExportEntry::Indirect { export_name, .. }
                | js_compile::bytecode::ExportEntry::StarAs { export_name, .. } => {
                    out.push(export_name.clone());
                }
                js_compile::bytecode::ExportEntry::Star { module: index } => {
                    let dependency =
                        module.with(|r| r.dependencies.get(*index as usize).cloned().flatten());
                    if let Some(dependency) = dependency {
                        let mut forwarded = Vec::new();
                        self.collect_export_names(&dependency, &mut forwarded, depth + 1)?;
                        // `export *` never forwards `default` (§16.2.1.6.2).
                        out.extend(forwarded.into_iter().filter(|name| name != "default"));
                    }
                }
            }
        }
        Ok(())
    }

    /// The namespace object of `module`, built on first use.
    ///
    /// An ordinary object with one read-only getter per export, inserted in sorted order.
    /// The getter reads the cell, which is what makes the binding live: `export let x`
    /// followed by `x = 1` in the exporting module is visible here without copying.
    pub fn module_namespace(&mut self, module: &Gc<ModuleRecord>) -> JsResult<Value> {
        if let Some(namespace) = module.with(|r| r.namespace.clone()) {
            return Ok(Value::Object(namespace));
        }
        let names = self.module_export_names(module)?;
        let proto = self.realm.object_proto.clone();
        let function_proto = self.realm.function_proto.clone();
        let namespace = self.new_object(Some(proto));
        for name in &names {
            let cell = self.resolve_export(module, name)?;
            let getter = JsObject::function(
                self.heap,
                Some(function_proto.clone()),
                Callable::ModuleBinding { cell },
            );
            namespace.define(
                name,
                Property {
                    value: None,
                    getter: Some(Value::Object(getter)),
                    setter: None,
                    writable: false,
                    enumerable: true,
                    configurable: false,
                },
            );
        }
        // A namespace reports `[object Module]`, and cannot gain properties.
        let tag_key = self.realm.well_known_key(WellKnown::ToStringTag);
        let tag = self.string("Module");
        namespace.define_key(&tag_key, Property::with_attributes(tag, false, false));
        self.object_prevent_extensions(&namespace)?;
        module.with_mut(|r| r.namespace = Some(namespace.clone()));
        Ok(Value::Object(namespace))
    }

    /// Run `module`'s body, evaluating its dependencies first.
    ///
    /// Post-order, once each: a module already evaluated is skipped, which is what
    /// §16.2.1.5.2.2's "evaluated or evaluating" guard amounts to, and what makes a cycle
    /// terminate instead of recursing.
    pub fn evaluate_module_body(&mut self, module: &Gc<ModuleRecord>) -> JsResult<()> {
        if module.with(|r| r.status) == ModuleStatus::Evaluated {
            return Ok(());
        }
        // Mark before recursing so a cycle stops here.
        module.with_mut(|r| r.status = ModuleStatus::Evaluated);

        let dependencies: Vec<Gc<ModuleRecord>> = module
            .with(|r| r.dependencies.clone())
            .into_iter()
            .flatten()
            .collect();
        for dependency in dependencies {
            self.evaluate_module_body(&dependency)?;
        }

        let Some(code) = module.with(|r| r.code.clone()) else {
            // A JSON/text module has no body; its value is already in its cell.
            return Ok(());
        };
        let name = module.with(|r| r.name.clone());
        let env = self.module_env(module);
        // The hoisted bindings already exist: `link_module` ran the prologue pass with a
        // truthy `this`, which is what the guard in the compiled prefix detects. Here the
        // receiver is `undefined`, so the body itself runs.
        //
        // A body with top-level `await` is compiled as an async function, so it answers a
        // promise instead of a value. The evaluation is then driven to completion here:
        // draining the queue is what advances the coroutine, and there is nothing else
        // pending at this point, since module evaluation is what started it.
        let async_body = code.code.kind.is_async();
        self.module_stack.push(name);
        let result = if async_body {
            self.run_async_body(code, env, Value::Undefined)
                .and_then(|promise| self.await_module_promise(promise))
        } else {
            self.execute(code, env, Value::Undefined, Vec::new(), Value::Undefined)
        };
        self.module_stack.pop();
        result.map(|_| ()).inspect_err(|_| {
            // A body that threw leaves the module unevaluated, so a later import of it
            // retries rather than silently reporting success.
            module.with_mut(|r| r.status = ModuleStatus::Linked);
        })
    }

    /// Drive a module's own promise to a settled state.
    ///
    /// `js_execute_sync_module` in the reference does the same thing and calls a pending
    /// promise an error ("promise is pending"); draining first is the difference, and it
    /// is correct here because nothing else can be waiting: this runs before the host has
    /// been given control, and the only queued jobs are the ones this body queued for
    /// itself.
    fn await_module_promise(&mut self, promise: Value) -> JsResult<Value> {
        let Value::Object(promise) = promise else {
            return Ok(Value::Undefined);
        };
        self.drain_jobs()?;
        let state = self.promise_state(&promise);
        let Some(state) = state else {
            return Ok(Value::Undefined);
        };
        let status = state.borrow().status.clone();
        match status {
            crate::object::PromiseStatus::Fulfilled(value) => Ok(value),
            crate::object::PromiseStatus::Rejected(reason) => Err(Signal::Throw(reason)),
            crate::object::PromiseStatus::Pending => Err(Signal::Throw(self.make_error(
                "TypeError",
                "promise is pending: top-level await never settled",
            ))),
        }
    }

    /// `import(specifier)`: load, link, evaluate — asynchronously.
    ///
    /// The work happens in a job, because §13.3.10.1 makes dynamic import settle through
    /// the promise machinery: `import("x").then(f)` must call `f` after the current
    /// script finishes, never inside the `import` expression.
    pub fn dynamic_import(&mut self, specifier: Value) -> JsResult<Value> {
        let promise = self.new_promise();
        let specifier = self.to_js_string(&specifier)?;
        // Resolved relative to the module that ran the import, which is the innermost
        // module body on the stack.
        let referrer = self
            .module_stack
            .last()
            .cloned()
            .or_else(|| self.main_module_name.clone())
            .unwrap_or_default();
        self.jobs.push_back(crate::interp::Job::Import {
            specifier,
            referrer,
            promise: promise.clone(),
        });
        Ok(Value::Object(promise))
    }

    /// Evaluate a pending dynamic import and settle its promise.
    pub fn run_dynamic_import(&mut self, specifier: &str, referrer: &str, promise: &Gc<JsObject>) {
        let outcome = self
            .load_module(specifier, &[], referrer)
            .and_then(|module| {
                self.link_module(&module)?;
                let name = module.with(|r| r.name.clone());
                self.module_stack.push(name);
                let evaluated = self.evaluate_module_body(&module);
                self.module_stack.pop();
                evaluated?;
                self.module_namespace(&module)
            });
        match outcome {
            Ok(namespace) => self.fulfill_promise(promise, namespace),
            Err(Signal::Throw(reason)) => self.reject_promise(promise, reason),
            Err(other) => {
                let _ = other;
            }
        }
    }
}

/// The registry key for a module: its canonical name plus the attributes that decided
/// what it means.
///
/// Without the attributes, `import m from "./x.json"` and
/// `import m from "./x.json" with { type: "text" }` would collide on one record and the
/// second import would silently receive the first one's value.
fn cache_key(name: &str, attributes: &[(String, String)]) -> String {
    if attributes.is_empty() {
        return name.to_string();
    }
    let mut key = name.to_string();
    for (key_name, value) in attributes {
        key.push('\u{0}');
        key.push_str(key_name);
        key.push('=');
        key.push_str(value);
    }
    key
}

/// Which kind of module `name` and `attributes` describe.
///
/// A `type` attribute wins; the `.json` suffix only decides when no attribute was given.
/// That is what makes `import m from "./m.json" with { type: "text" }` a string while
/// `import m from "./m.json"` is an object.
fn module_kind(name: &str, attributes: &[(String, String)]) -> Result<ModuleKind, String> {
    let declared = attributes
        .iter()
        .find(|(key, _)| key == "type")
        .map(|(_, value)| value.as_str());
    let kind = match declared {
        // `json5` is accepted by the reference as an alias for a dialect that permits
        // comments and trailing commas; this parser is strict JSON, so a document that
        // needs the extension fails loudly rather than silently parsing differently.
        Some("json") | Some("json5") => ModuleKind::Json,
        Some("text") => ModuleKind::Text,
        Some("bytes") => ModuleKind::Bytes,
        // An unknown type is an error rather than a fallback to JavaScript: silently
        // evaluating `with { type: "cbor" }` as code would run a data file.
        Some(other) => return Err(format!("unsupported module type: '{other}'")),
        None => {
            if name.ends_with(".json") {
                ModuleKind::Json
            } else {
                ModuleKind::JavaScript
            }
        }
    };
    Ok(kind)
}
