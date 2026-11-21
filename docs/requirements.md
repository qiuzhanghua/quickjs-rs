# quickjs-rs 需求分析（纯 Rust 重写 QuickJS）

> 状态：v0.4 —— 需求分析已确认（§8），M0 与 M1a（词法器）已完成，下一步 M1b（AST + 解析器）
> 上游基线：`https://github.com/bellard/quickjs` `master`，`VERSION = 2026-06-04`

---

## 1. 背景与事实基线

目标：用**纯 Rust**（不链接任何 C 代码、不依赖 C 编译器）实现一个功能等价于 QuickJS 的 JavaScript 引擎，
最终能替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译、以及其他语言绑定）。

上游规模（实测 `wc -l`，2026-06-04 版本）：

| 组件 | 文件 | 行数 | 职责 |
|---|---|---|---|
| 引擎核心 | `quickjs.c` | 65,793 | 词法、语法、字节码编译器、VM、对象模型、内建库、GC、模块、Promise |
| 标准库 | `quickjs-libc.c` | 4,506 | `std`/`os` 模块、事件循环、Worker、模块加载器 |
| 正则 | `libregexp.c` + `.h` | 3,448 | 自有 bytecode 正则引擎（回溯式） |
| 数字↔字符串 | `dtoa.c` | 1,620 | 最短往返 float 打印/解析 |
| Unicode | `libunicode.c` + `libunicode-table.h` | 2,131 + 5,212 | 大小写、规范化、脚本/属性表 |
| Intl（实验） | `intl/*.c` | 12 文件 | ECMA-402 + CLDR `locale-data.c` |
| Temporal（实验） | `temporal/*.c` | 15 文件 | ES2024 Temporal + `tzdata_generated.c` |
| CLI / REPL | `qjs.c` + `repl.js` | 587 + 39KB JS | 交互式解释器 |
| AOT | `qjsc.c` | 920 | 源码 → 独立可执行文件 |
| 字节码/原子定义 | `quickjs-opcode.h` / `quickjs-atom.h` | 252 opcode / 251 atom | |
| 公共 C API | `quickjs.h` | 1,289 / **233 个导出符号** | |
| 内建函数 | `quickjs.c` | 176 个 `JS_CFUNC_DEF` + 44 个 getter/setter | |

语言能力（`doc/quickjs.texi` + `Changelog`）：
- 近乎完整 **ES2025**（含 Annex B、Unicode），未实现：尾调用、`Atomics.waitAsync`。
- 全量 ES6 模块（含 TLA、动态 `import()`、JSON 模块 + import attributes、循环导入）。
- 自有字节码格式 + `JS_WriteObject/JS_ReadObject` 序列化（qjsc 的 AOT 基础）。
- GC = **引用计数 + 循环回收**（确定性释放、内存占用低）。
- Temporal / Intl 需 `--experimental` 打开，默认 `qjs` 中不存在（已实测 `typeof Intl === "undefined"`）。

本机可构建出 C 版参考实现（`make qjs qjsc`，clang -O2，~50s 成功），
可作为 **差分测试的 oracle**：`tests/microbench.js` 总耗时约 4254（相对单位）。

---

## 2. "替换 quickjs" 的三种语义（决定整体架构）

需要先明确要替换哪一层，直接决定公共 API 形态：

| 方案 | 含义 | 一致性要求 | 代价 |
|---|---|---|---|
| A. 替换**引擎**（推荐） | 提供惯用 Rust API（`Runtime`/`Context`/`Value`/`Object`），同时提供可选的 `extern "C"` 兼容层 | 语义一致 + 可选符号兼容 | 中 |
| B. 1:1 复刻 **C API** | `quickjs.h` 233 个符号逐个 `extern "C"` 导出，产出 `libquickjs.dylib/.so/.a` | ABI 级 | 高（值表示/GC 语义被 C 布局绑死） |
| C. 兼容 **rquickjs** API | 让现有 `rquickjs` 用户零改动迁移 | Rust 层 API 级 | 中（受制于他人设计，且需跟随其版本） |

**已确认（v0.1 评审）：采用方案 A**，B 设计成可插拔的独立 crate（`quickjs-capi`，feature `c-api`），
C（rquickjs 兼容）暂不纳入范围，只在公共 API 设计上避免明显冲突。

---

## 3. 功能需求（FR）

按依赖顺序编号，每条都给出验收方式。验收口径统一为：**与 C 版 `qjs` 的差分测试通过** + 上游 `tests/*.js` 通过 + test262 对应用例通过。

### FR-1 词法与语法分析
- 完整 ES2025 词法：标识符（含 Unicode ID_Start/ID_Continue、`\u{}` 转义）、数字字面量（十进制/十六进制/八进制/二进制/数字分隔符）、BigInt 后缀、
  字符串/模板字面量、正则字面量、注释、ASIC/全角空白、hashbang。
- 完整语法：表达式（含 `**`、`??`、`?.`、解构、箭头、async 箭头、`yield`）、语句、类（字段/私有/静态块/装饰器不支持）、
  模块语法（`import`/`export`/`import.meta`/`import attributes`/`export * as`）、`using`/`await using`（显式资源管理）。
- 严格模式/非严格模式、保留字、早期错误（early errors）与准确报错位置（行 + 列）。
- 验收：语法错误用例的错误类型与消息类别一致；位置（line/col）与 C 版一致。

### FR-2 编译（AST → 字节码）
- 作用域解析：`var`/`let`/`const`/函数提升、TDZ（`check_uninitialized`）、闭包捕获（`closure_var`）、`with`/`eval` 打破静态作用域。
- 控制流：标签、`break`/`continue`、`try/catch/finally`（含 `catch_offset`、异常表）、generator/async 的挂起点。
- 优化 pass（对标上游）：常量折叠、peephole、死代码、`typeof` 特化、简单表达式合并。
- 输出：稳定的自有字节码（magic + version + 函数原型 + 常量池 + atom 表 + 调试信息 line/col + strip 选项）。
- 验收：字节码 dump 可读；`--strip-source` 后仍可执行；序列化往返稳定（同一输入两次编译产物字节一致）。

### FR-3 虚拟机（执行语义）
- 寄存器/栈式指令派发（沿用上游 252 opcode 的分类，命名可调整）；直接线程化不安全，改用 `match` + 分层 hot-path。
- 调用：普通/构造/`new.target`/`super`/`eval`（直接+间接）/`Function` 构造器；`arguments`（mapped + unmapped）。
- 异常：`throw`/`catch`/`finally`/`Error.cause`/`error.stack`（accessor）/`AggregateError`/`Error.isError`。
- Promise：微任务队列、`then/catch/finally`、`all/allSettled/any/race/try/withResolvers`、未处理 rejection 跟踪（默认致命，同上游）。
- 生成器/异步：状态机挂起/恢复、`AsyncGenerator`、`for await`、async 上下文传播。
- 对象模型：隐藏类无关的属性有序表（插入序 + 数组索引快路径）、原型链、`Proxy`（全 trap + revocable）、
  `Reflect`、私有字段/方法（`#x`）、`Symbol.toPrimitive` 等 Well-known symbols、`Object.freeze/seal`。
- 迭代协议：`Symbol.iterator`/`asyncIterator`、迭代器 helper（`Iterator.prototype.map/take/drop/...`）、
  `Iterator.concat`、`Iterator.from`。
- 内建对象全量：`Object Function Array String Number Boolean Math JSON Date RegExp Error` 家族、
  `Map/Set/WeakMap/WeakSet`（含 upsert：`getOrInsert` 等）、`Symbol`、`BigInt`、
  `ArrayBuffer/SharedArrayBuffer/DataView/TypedArray`（含 `Float16Array`、resizable buffer、`transfer`、base64/hex 编解码）、
  `Atomics`（含 `pause`）、`WeakRef`/`FinalizationRegistry`、`Intl`（FR-9，已确认纳入范围）。
- 验收：test262 选中 ES2025 features 的通过率 ≥ 95%（上游 ~100%），`tests/test_builtin.js` 等全通过。

### FR-4 模块系统
- ES 模块加载器：路径规范化、`import.meta`、命名空间对象、循环依赖（`tests/test_cyclic_import.js`）、TLA。
- 动态 `import()`、JSON 模块、import attributes。
- 宿主可插拔 `ModuleLoader` trait（对标 `JS_SetModuleLoaderFunc2`、`JS_ResolveModule`）。
- C 模块等价物：`quickjs::c_module!` 宏注册原生模块（对标 `JS_NewCModule`/`JS_AddModuleExport`）。
- 验收：模块相关测试与上游行为一致；错误（不可解析/循环未完成）信息一致。

### FR-5 正则引擎（`js-regexp`）
- 自有回溯 VM（对标 libregexp 3.4k 行）：字符类、Unicode 属性转义（`\p{Script=...}`）、`u`/`v` 标志、
  命名与重复命名捕获组、modifiers（`(?i:...)`）、lookbehind、`RegExp.escape`、`lastIndex`/sticky。
- 依赖 Unicode 表（FR-8）。
- 验收：test262 `RegExp` 目录通过率 + 与 C 版 `exec` 结果（捕获组/`indices`）逐字段 diff。

### FR-6 数字与字符串转换（`js-dtoa`）
- 最短往返 float64 打印（Grisu/Ryu 级别）、`Number.prototype.toString(radix)` 全 radix 域、`parseFloat`/`parseInt`、
  `Number()` 字符串文法（含 `+0`/`-0`、`NaN`、`Infinity`、下划线、前后空白）。
- 验收：与 C 版对大随机样本（含次正规数、极大指数）输出逐位一致。

### FR-7 字节码序列化与 AOT
- `write_object`/`read_object`（等价 `JS_WriteObject2`）：GC 无关的自描述格式、BJSON（`std` 中 `bjson`）。
- `qjsc` 等价工具：把 JS 编译成**不依赖外部文件**的可执行文件。纯 Rust 方案：生成一个临时 Rust crate
  （`include_bytes!` 内嵌字节码 + 极简 runner，链接 `quickjs` rlib）→ 调用系统 `cargo`/`rustc` 产出二进制。
  可选进阶：自建 ELF/Mach-O 包装器（零依赖，但平台相关，列为 stretch）。
- 验收：`qjsc` 产物在目标平台可运行且与 `qjs script.js` 输出一致。

### FR-8 Unicode 与日期
- Unicode 表由脚本生成（对标 `unicode_gen.c`）：大小写映射、`NFKC`/`NFC` 规范化、`ID_Start/ID_Continue`、
  `Script`/`General_Category`/`Emoji` 属性、`String.prototype.{isWellFormed,toWellFormed,normalize}`。
- `Date`：全 ES2025 语义（含 `toString` 依赖时区）、`Date.parse` 各 ISO 形式。
- 验收：Unicode 用例 + `Date` 边界用例（DST、负年份、极值 ±8.64e15）与 C 版一致。

### FR-9 Temporal 与 Intl（已确认纳入范围，但排在最后：M13/M14）
- Temporal：`Instant/Duration/PlainDate/PlainTime/PlainDateTime/PlainYearMonth/PlainMonthDay/ZonedDateTime/Now`，
  需要内置 tzdata（对标 `tzdata_generated.c`）+ 日历（`iso8601`，可扩展）。
- Intl：`Collator/NumberFormat/DateTimeFormat/PluralRules/ListFormat/RelativeTimeFormat/Segmenter/DisplayNames/DurationFormat`，
  需要 CLDR 子集（对标 `locale-data.c`，体积数十 KB ~ 数 MB，做成独立 crate + 数据 feature）。
- 验收：`tests/test_intl.js` + Temporal 官方测试子集。
- 交付形态：默认关闭（与上游 `--experimental` 对齐），通过 `feature = "temporal"` / `"intl"` 开启；
  数据放独立 crate（`js-tzdata`、`js-intl-data`），不阻塞主线里程碑，也不污染核心引擎包体。

### FR-10 标准库（`std` / `os`）
- 模块：`std`（`exit`、`evalScript`、`loadScript`、`open/read/write`、`puts/printf`、`parseExtJSON`、`bjson`、
  `strerror`、`pipe`、`uuid`、`setTimeout/clearTimeout`、`Worker`、`os`（`open/read/write/close/seek/stat/...`、`exec`、
  `signal`、`setReadHandler`、`getenv`、`platform`、`cwd`、`mkdir`、`realpath`、`sleep`、`now`）。
- 事件循环：`setTimeout`/`setImmediate`/`os.setReadHandler` + Promise 微任务 + `Worker`（线程 + `SharedArrayBuffer`）。
- 验收：`tests/test_std.js`、`test_worker*.js` 通过；`os` 行为与平台一致。

### FR-11 CLI 与 REPL
- `qjs` 等价：全部选项（`-e/-m/--std/-I/-T/-d/-s/--memory-limit/--stack-size/--experimental/-q`…）行为一致。
- 交互式：多行编辑、补全、语法高亮、历史、`std.prettyPrint` 风格对象打印、promise rejection 追踪提示。
- 验收：选项矩阵测试（同脚本同选项输出 diff 一致）。

### FR-12 Rust 公共 API（替换交付面）
- 值层：`Value`（Copy、16 字节）、`Atom`、`Object`、`Function`、`Exception`；`OwnedValue` 用于跨作用域持有。
- 生命周期：`Runtime`（GC/内存上限/中断/作业队列）→ `Context`（全局对象/内建注入/模块表）→ 句柄。
- 宿主接口：`ClassDef`（含 exotic methods、opaque 回调、finalizer）、`CFunction`、`PropertyEnum`、
  `InterruptHandler`、`PromiseRejectionTracker`、`MemoryLimit`、`StackLimit`、`LocaleCallbacks`。
- 便捷层（对齐 rquickjs 用户习惯但不复制其设计）：`Context::eval`、`Function::call`、`Object::set/get`、
  `Context::coerce_*`、`#[derive(Trace)]`-风格宏、`module!` 宏。
- 错误：`Result<T, Exception>`；`Exception` 可 `catch` 成 Rust 错误或重新抛出。
- 验收：公共 API 0 `unsafe`；`#![deny(unsafe_op_in_unsafe_fn)]`；文档示例全部 doctest 通过。

### FR-13 C ABI 兼容层（`quickjs-capi`，feature `c-api`，M12）
- 导出 `quickjs.h` 的 233 个符号（`extern "C"`），`JSValue` 按上游当前平台布局（64 位 NaN boxing 或 tag+union）实现。
- 目的：让现有 C/其他语言绑定（如 `quickjs-ng` 用户、Go/Python 绑定）可直接链接替换。
- 代价：值表示被 ABI 绑死 → 放在独立 crate，内部用 `Value ↔ JSValue` 转换，仅牺牲少量性能。
- 验收：用上游 `examples/*.c`、`tests/bjson.c` 编译链接通过并运行一致。

---

## 4. 非功能需求（NFR）

| 编号 | 要求 | 验收方式 |
|---|---|---|
| NFR-1 | **纯 Rust**：默认构建不调用 C 编译器、不链接 C 代码；`cargo build` 即可 | CI 在无 clang 容器内构建通过 |
| NFR-2 | 性能：`tests/microbench.js` ≥ C 版 **0.5×**（阶段目标 0.3× → 0.7×）；启动 < 2 ms | 与本机 `qjs` 基准对比脚本 |
| NFR-3 | 内存：空 Runtime < 1 MB；`--memory-limit` 精确生效（超限抛 `InternalError: out of memory`） | 内存测试 + `-d` 统计对比 |
| NFR-4 | 确定性释放：`Rc` 语义，作用域结束即释放；循环引用由周期性 collector 回收（对齐上游 RD/GC 行为） | 析构计数器测试（finalizer 触发时机） |
| NFR-5 | 无 panic 逃逸：宿主回调 panic 转为 `InternalError`；`catch_unwind` 边界；`--stack-size` 防爆栈 | fuzz + 深递归测试 |
| NFR-6 | 安全：`unsafe` 只允许在 `gc`/`atomics`/`typed_array`/`c-api` 模块，逐处 `// SAFETY:` | `#![warn(unsafe_op_in_unsafe_fn)]` + review checklist |
| NFR-7 | 依赖最小化：runtime 零第三方 crate（`libc` 也避免，用 `std` + 少量 `sys` 封装）；dev-deps 不限 | `cargo tree` 审计 |
| NFR-8 | 平台：macOS(arm64/x64)、Linux(x64/arm64)、Windows(msvc)；WASM 列为可选 | CI 矩阵 |
| NFR-9 | 可调试：`--dump`、字节码反汇编、`trace` 分配跟踪、错误栈含行列与函数名 | 手工验收 |
| NFR-10 | 可测试：差分 oracle 框架（跑 C 版 `qjs` vs Rust `qjs`）+ test262 harness + fuzz 目标 | `cargo test` 一键运行 |
| NFR-11 | 可嵌入：`no_std` 不支持（明确），但 `std` 访问集中在 `sys` 层，便于后续移植 | 架构评审 |
| NFR-12 | MSRV：edition 2024（rustc ≥ 1.85） | CI |

---

## 5. 架构方案

```
crates/
  js-gc/           # Gc<T>、Trace、循环回收、弱引用、finalizer
  js-value/        # Value/Atom（NaN-boxing 可选 feature）、字符串（原子/rope）、属性表
  js-parse/        # 词法、AST、解析器、早期错误
  js-compile/      # 作用域解析 → 字节码、常量池、调试信息、序列化格式
  js-vm/           # 指令派发、调用栈、迭代器、Promise 队列、异常、中断
  js-builtins/     # 内建对象/函数（按 ES 章节分子模块）
  js-dtoa/         # 数字↔字符串
  js-regexp/       # 正则引擎（自有 bytecode）
  js-unicode/      # 生成表 + 属性查询 + 规范化
  js-std/          # std / os / Worker / 事件循环
  quickjs/         # ★ 公共 API（Runtime/Context/Value/ClassDef/module!）
  qjs/             # CLI + REPL
  qjsc/            # AOT 编译器
  quickjs-capi/    # C ABI 兼容层（M12）
  quickjs-conformance/ # 差分测试 harness（dev，与 C 版 qjs 对拍）
```

关键设计决策：

1. **值表示**：`#[repr(C)] enum Value { Undefined, Null, Bool(bool), Int(i32), Float(f64), ShortBigInt(i64),
   String(Gc<String>), BigInt(Gc<BigInt>), Object(Gc<Object>) }`，16 字节。
   NaN-boxing 作为 `feature = "nanbox"` 的后优化（需 ABI 兼容时才启用，见 FR-13）。
2. **GC**：`Gc<T>` = 非原子引用计数 + `Trace` + 周期性 trial-deletion 循环回收（boa 已验证的路线），
   `WeakGc` 供 `WeakRef`/`WeakMap`/`FinalizationRegistry` 使用。保留 `JS_RunGC()` / `set_gc_threshold` 等价 API。
3. **执行模型**：字节码 VM（不采用 AST 解释），因为需要 qjsc/AOT、`eval` 性能与上游字节码语义对齐。
4. **原子（Atom）**：`u32` 全局 intern 表（每 Runtime 一份）+ 常见 atom 静态预置（对标 `quickjs-atom.h` 251 个）。
5. **异常模型**：`Exception` 携带 JS 值；VM 内部用 `ControlFlow` 处理 `try/finally`/`break` 目标，避免 Rust `panic`。
6. **`unsafe` 边界**：GC 内部（`UnsafeCell` + 手动引用计数）、TypedArray 视图、`SharedArrayBuffer`、C ABI。
7. **线程/Atomics**：`Runtime`/`Context` 为 `!Send`/`!Sync`；`SharedArrayBuffer` 用 `Arc<SharedMem>`；
   `Atomics.wait` 需真实线程阻塞 → Worker 线程池 + 条件变量（PR：列为高风险项）。

---

## 6. 里程碑（每阶段可独立验收，输出可运行产物）

| 里程碑 | 内容 | 出口标准（可运行/可验收） |
|---|---|---|
| M0 ✅ | 工作区骨架 + 差分 oracle 框架 + `js-gc` | 已完成：`js-gc` 16 个测试全绿；`qjs --help`/用法错误与 C 版**逐字节一致**；harness 自检通过 |
| M1a ✅ | 词法分析器 | 已完成：33 个单元测试；35 个数值字面量与 C 版**逐位相同**（DataView 取位对比）；13 个字符串字面量解码一致 |
| M1b | AST + 解析器 + early errors | 所有上游 JS 测试文件能"解析成功/失败符合预期" |
| M2 | 字节码编译器 + 作用域解析 + 序列化 | 反汇编输出可读；往返稳定 |
| M3 | VM 核心（调用/对象/异常/闭包/原型）+ ~60 opcode | 能跑递归/fib/数组/闭包脚本 |
| M4 | 内建库第一波（Object/Function/Array/String/Number/Math/JSON/Error/Date） | `tests/test_builtin.js` 大部分通过 |
| M5 | ES6+ 语法运行期（class/generator/async/迭代/解构/模板/Proxy） | `test_language.js`、`test_closure.js` 通过 |
| M6 | 模块系统 + 动态 import + JSON 模块 | `test_cyclic_import.js`、`hello_module.js` 通过 |
| M7 | 正则引擎 + Unicode 表 | test262 RegExp 通过率达标 |
| M8 | BigInt / TypedArray / ArrayBuffer / Atomics / WeakRef | test262 对应目录达标 |
| M9 | `std`/`os`/Worker/事件循环 + CLI + REPL | `test_std.js`、`test_worker.js`、CLI 选项矩阵 |
| M10 | 字节码 AOT（qjsc 等价）+ BJSON | 产物可执行且输出一致 |
| M11 | 性能优化（内联缓存、NaN boxing、字符串 rope、peephole） | microbench ≥ 0.5× C 版 |
| M12 | C ABI 兼容层（`quickjs-capi`，233 个符号） | 上游 `examples/*.c`、`tests/bjson.c` 链接后运行一致 |
| M13 | Temporal（tzdata + ISO 日历，9 个类型） | Temporal 官方测试子集；`--experimental` 行为对齐 |
| M14 | Intl / ECMA-402（CLDR 子集，9 个构造器） | `tests/test_intl.js` 通过；locale 敏感输出与 C 版 diff 一致 |

规模量级（Rust 代码行数估算，仅供参考）：核心 ~45k、内建 ~25k、正则 ~8k、Unicode ~4k（大部分是生成表）、
dtoa ~2k、std ~6k、CLI/REPL ~3k、Intl ~10k / Temporal ~12k、C ABI 层 ~4k。**总计 ~100k+ 行，属于多阶段长期工程**，
不可能单会话完成；上下文（M0–M3 骨架 + 差分框架）必须最先落地，否则后续每步都无法验证。

---

## 7. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 规模巨大（~100k 行） | 交付周期 | 里程碑切片 + 每片可运行；差分测试作为护栏 |
| GC 语义错（FinalizationRegistry 时机、循环回收） | 内存泄漏/测试不稳 | M0 先写 GC 单测；借鉴 boa 的 cycle collector |
| Intl/Temporal 数据体积（CLDR/tzdata） | 包体膨胀 | 独立 crate + 数据 feature；默认关闭（同上游 `--experimental`） |
| RegExp `v` 标志/Unicode 属性 | 通过率低 | 直接生成表 + 逐用例 diff |
| 性能不达标（Rust 解释器开销） | 替换价值下降 | NaN boxing + 指令分派优化 + 内联缓存；先正确后快 |
| C ABI 兼容与内部表示冲突 | 架构返工 | 一开始就把 ABI 层做成独立 crate（转换边界） |
| `SharedArrayBuffer`/`Atomics.wait` 跨线程 | 复杂性与 UB 风险 | 显式 `Arc` + 条件变量；`unsafe` 集中 + loom 测试 |
| 上游语义无文档（隐式行为） | 一致性偏差 | 一律以 C 版 `qjs` 实测输出为 oracle（本机可构建） |

---

## 8. 已确认决策（v0.1 评审通过）

| # | 决策点 | 结论 | 影响 |
|---|---|---|---|
| 1 | API 形态 | **方案 A**：惯用 Rust API 为主（`Runtime`/`Context`/`Value`/`ClassDef`/`module!`），C ABI 兼容层做独立 crate（M12） | 核心表示不被 C 布局绑死；ABI 层只做值转换 |
| 2 | 功能范围 | **全量**：核心 ES2025 + 模块 + BigInt/TypedArray/Atomics/WeakRef，**含 Temporal 与 Intl（ECMA-402/CLDR）** | Temporal/Intl 排 M13/M14，独立数据 crate + feature，不阻塞主线 |
| 3 | 字节码 | **自有稳定格式**（magic + version + 稳定序列化），不与 C 版二进制互通；保留 `write_object`/`read_object` 语义与 qjsc AOT | opcode 布局可自由优化；不能直接执行 C 版编译产物 |
| 4 | GC 路线 | **`Rc` + 循环回收**（引用计数 + trial-deletion cycle collector），确定性释放，对齐上游语义 | `Gc<T>`/`WeakGc<T>`/finalizer 必须在 M0 先落地并单测 |

遗留待定（不阻塞 M0–M3）：

- 是否要求 `wasm32` 目标（影响 `std::thread` 与 `Atomics` 设计）。
- `qjsc` 产物形态：M10 先做「生成 Rust 源 + `cargo build`」，自建 ELF/Mach-O 包装器列为 stretch。
- 是否发布 crates.io、版本号策略（`0.x` 与上游 `VERSION` 的映射）。

---

## 9. 参考（本地可用）

- C 版源码与参考二进制：`/tmp/quickjs-master`（`make qjs qjsc` 已构建，`qjs --help` 选项可枚举）
  重新获取（本机需直连、不经代理）：
  ```sh
  curl -o qjs.tar.gz https://codeload.github.com/bellard/quickjs/tar.gz/refs/heads/master && tar xzf qjs.tar.gz
  cd quickjs-master && make -j8 qjs qjsc      # 参考 oracle
  ```
- 一致性基线命令：`./qjs tests/microbench.js`（total ≈ 4254）、`./qjs tests/test_builtin.js`、
  `./qjs tests/test_language.js`、`./qjs --experimental -e 'print(typeof Temporal)'`
- test262：需 `git clone https://github.com/tc39/test262`（`test262.conf` 已给出所选 features 清单）
