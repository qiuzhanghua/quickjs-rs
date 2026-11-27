# quickjs-rs

用**纯 Rust** 重写 [bellard/quickjs](https://github.com/bellard/quickjs)（基线 `VERSION = 2026-06-04`），
目标：不依赖 C 代码即可构建，功能/语义等价，可替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译）。

- 📄 需求分析：[`docs/requirements.md`](docs/requirements.md)（v0.5，关键决策已确认）
- 📌 当前版本：**v0.6.1** —— 标准库补到位，test262 可运行用例通过率 42.7% → 83.6%：
  class、解构、generator、`Proxy`、`Promise`/async、`for await`；ES 模块（`import`/`export`、
  循环导入、`import()`、JSON/text 模块）；`RegExp`、`Symbol`、`Map`/`Set`、`BigInt`、
  TypedArray（`DataView`、29 个数组方法中的 28 个、`Symbol.iterator`）、`Iterator` helpers、
  `Array.fromAsync`
- 🚧 当前进度：**M0–M6 已完成**；M7（正则引擎 + Unicode 表）起未开始
- 📄 变更记录：[`CHANGELOG.md`](CHANGELOG.md)

## 已确认决策

| 决策点 | 结论 |
|---|---|
| API 形态 | 惯用 Rust API 为主；C ABI 兼容层做独立 crate（M12） |
| 功能范围 | 全量：核心 ES2025 + Temporal + Intl（ECMA-402/CLDR，独立 feature） |
| 字节码 | 自有稳定格式，不与 C 版二进制互通；保留 qjsc AOT |
| GC | `Rc` + 循环回收（引用计数 + trial-deletion），确定性释放 |

## 里程碑状态

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | 工作区骨架、`quickjs-gc`（Gc/WeakGc/循环回收）、差分测试框架、`qjs` 选项面 | ✅ |
| M1a | 词法分析器（ES2025 全量词法） | ✅ |
| M1b | AST + 解析器 + early errors + 作用域分析 | ✅ |
| M2 | 字节码编译器 + 作用域解析 + 序列化 | ✅ 逐 opcode 差分通过 |
| M3 | VM 核心（调用/对象/异常/闭包/原型） | ✅ `qjs` 能执行 JS |
| M4 | 内建库第一波（Object/Function/Array/String/Number/Math/JSON/Error/Reflect） | ✅ `test_builtin.js` 的 M4 部分 229/237 语句通过 |
| M5 | ES6+ 运行期（class/generator/async/迭代/解构/模板/Proxy） | ✅ 残留：私有类字段 `#x` 编译器报错 |
| M6 | 模块系统 + 动态 `import()` + JSON/text 模块 | ✅ `hello_module.js` 与参考逐字节一致 |
| M7 | 正则引擎 + Unicode 表 | 🟡 `RegExp` 可用，Unicode 属性表（`\p{…}`）已补（606/613）；缺 `RegExp.escape`、lookbehind、`v` 模式的 emoji 字符串属性 |
| M8 | BigInt / TypedArray / ArrayBuffer / WeakRef | 🟡 BigInt/TypedArray/DataView 可用，`WeakMap`/`WeakSet`/`WeakRef`/`FinalizationRegistry` 已补；缺 resizable `ArrayBuffer` 与 `TypedArray` 个别方法 |
| ~~M8.5~~ | ~~`Atomics` / `SharedArrayBuffer`~~ | ❌ **不做**（见下） |
| **M9** | **`std`/`os` 宿主模块 + 事件循环 + CLI + REPL** | ⬜ **下一项** |
| M10 | 字节码 AOT（qjsc 等价）+ BJSON | ⬜ |
| M11 | 性能优化（内联缓存、NaN boxing、字符串 rope、peephole） | ⬜ |
| M12 | C ABI 层 | ⬜ |
| ~~M13~~ | ~~`Temporal`~~ | ❌ **不做**（见下） |
| ~~M14~~ | ~~`Intl`~~ | ❌ **不做**（见下） |

**三项明确不做，理由不是"难"，而是"做了也不是真的"。**

- **`Atomics` / `SharedArrayBuffer`**：本引擎单线程，没有 Worker。`Atomics.wait` 无法真的
  阻塞，`Atomics.notify` 没有等待者——实现出来的会是一个永不执行其语义的 API 外壳。这正是
  本项目一直在修的那类问题（"接受即忽略，等于假的安全"）。487 条用例继续按 feature 跳过。
- **`Temporal`**（4624 条）：独立于引擎核心的巨型 API，对"qjs 能不能跑真实程序"几乎无影响
  ——真实程序极少用。它与 M9 争夺同一段时间，而 M9 决定这个引擎能不能被嵌入使用。
- **`Intl`**：需要 CLDR 数据与 ICU 级别的区域设置支持，纯 Rust 零依赖的前提不成立。

**顺序也改了。** 原先 M9 排在 M8 之后、M8 又以 `Atomics` 收尾，于是"能嵌入当脚本引擎用"
（读文件、定时器、事件循环）一直被一件做不真的东西挡着。现在 M9 提前到 M8 收尾之后。

## 已交付的结构

```
crates/quickjs-gc/           引用计数 + 循环回收（16 个测试）           lib: js_gc
crates/quickjs-parse/        词法 + AST + 解析器 + 差分测试            lib: js_parse
crates/quickjs-compile/      字节码编译器 + 反汇编 + 序列化            lib: js_compile
crates/quickjs-pure/         VM + 对象模型 + Value + 公共 API        lib: quickjs
crates/quickjs-cli/          CLI：选项面与上游逐字节一致，且真正执行   bin: qjs
crates/quickjs-conformance/  差分测试 harness（候选 vs C 参考实现）
scripts/setup-oracle.sh      下载并构建 C 参考实现（oracle）
scripts/check.sh             提交前的门禁：fmt + clippy -D warnings + test
```

包名带 `quickjs-` 前缀是因为 `quickjs` / `qjs` / `quickjs-rs` 在 crates.io 上
已被他人占用；lib 名与二进制名保持短名，`use quickjs::…` 与 `qjs` 命令不变。
**已发布到 crates.io（v0.6.1）**：`quickjs-pure` 是门面
crate（lib 名仍是 `quickjs`），`quickjs-gc`/`quickjs-parse`/`quickjs-regexp`/
`quickjs-compile` 是其依赖。`quickjs-cli` 与
`quickjs-conformance` 标了 `publish = false`，分别因为会跟 C 版 `qjs` 抢命令名、以及
只是 dev-dependency。用法：

```toml
[dependencies]
quickjs = { package = "quickjs-pure", version = "0.6" }
```

语言与标准库：`Object`、`Function`、`Array`、`String`、`Number`、`Boolean`、`Math`、
`JSON`、`Reflect`、`Date`、`RegExp`、`Symbol`、`Map`/`Set`、`BigInt`、TypedArray、
错误类，以及 class/解构/generator/`Proxy`/`Promise`/async/`for await` 与 ES 模块。
`error.stack` 精确到 `file:line:column`。

**已知缺口**（都是响亮失败，不会给错答案）：

| 缺口 | 行为 |
|---|---|
| `std`/`os` 宿主模块、事件循环、Worker（M9） | `--std` 报 `not implemented yet`；`setTimeout` 未定义 |
| `--memory-limit` / `--stack-size` | 报 `not implemented yet`（此前接受即忽略，等于假的安全） |
| `Atomics`/`SharedArrayBuffer`（M8） | `undefined`；相关用例按 feature 跳过 |
| `RegExp.escape`、lookbehind、`v` 模式的 emoji 字符串属性（M7） | `RegExp.escape` 为 `undefined`；lookbehind 报 `lookbehind and named groups are not supported`；`\p{Basic_Emoji}` 报 `invalid Unicode property`（`\p{ASCII_Hex_Digit}` 等单码点属性已可用） |
| resizable `ArrayBuffer`（`rab.resize`） | `undefined`，test262 里 76 条据此失败 |
| 私有类字段 `#x`（M5 残留） | 编译器报 `private class fields are not supported` |
| `Uint8Array.prototype.toSpliced`  等个别方法 | 为 `undefined`（TypedArray 的 29 个数组方法里已有 28 个） |
| 通用语法错误的输出格式 | 我们 `<file>: <line>: <col>: <msg>`，参考 `SyntaxError: <msg>` + `at` 行 |
| `Function.prototype.toString` | 用户函数返回 `[native code]` 而不是源码文本 |

`Iterator` helpers、`Array.fromAsync`、`DataView`、TypedArray 的数组方法与迭代
都已可用——它们不在上表里。

完整清单（含每条的排期）见 `docs/requirements.md` 的「已知缺口」与
`CHANGELOG.md`；发布计划见 `docs/requirements.md` 的「发布策略」。

## 开发

```sh
./scripts/setup-oracle.sh     # 一次性：构建 C 参考实现到 /tmp/quickjs-master（需网络 + C 工具链）
./scripts/check.sh            # fmt + clippy + test + 上游差分（提交前必跑）
cargo run -p qjs -- --help    # 与上游 qjs --help 逐字节一致
```

差分测试会自动寻找 `$QJS_ORACLE`、`/tmp/quickjs-master/qjs` 等路径；找不到参考实现时
相关用例打印 `skipping:` 而不是失败。引擎尚未能执行 JS 时，`qjs` 以退出码 2 + stderr
`not implemented yet` 表示“待实现”，harness 记为 *pending* 而非失败。

## 与 C 版的一致性验证

| 层面 | 差分手段 | 结果（2026-11-26 实测） |
|---|---|---|
| CLI | `qjs --help` / 用法错误 | stdout+stderr+退出码**逐字节一致** |
| `scriptArgs` | `-e`／脚本／`-m` 三种形态 | 数组内容一致（含 `-e` 后裸参数不当作脚本） |
| 数值字面量 | `DataView` 取 f64 两半 | **36** 个字面量**逐位相同** |
| 字符串字面量 | `JSON.stringify` 往返 | 13 个字面量解码一致 |
| 语法接受性 | `qjsc -c`（编译不执行）退出码 | 104 有效 + **56** 无效 + **38** 模块用例一致 |
| 声明冲突 | 同上 | **55** 条作用域冲突用例一致（27 拒绝 + 28 接受，标签本身也被校验） |
| 上游语料 | 同上 | `tests/` 的 **8** 个文件 + `examples/` 的 **1** 个一致（见下） |
| 字节码 | `qjsc-dump`（`-DDUMP_BYTECODE=1`）逐 opcode 反汇编 | **366** 条用例逐字节比较，0 差异 |
| TDZ/作用域 | 同上 + **27** 条不变量测试 | 作用域链、check 操作码、捕获与关闭全部对齐 |
| break/continue/标签 | 同上 | **20** 条用例一致；跳转目标栈、多层标签、do-while 的 test 与 for 的 update 全对 |
| switch | 同上 | 18 条用例一致；fallthrough、`default` 任意位置、空 `case` 链全对 |
| try/catch/finally | 同上 | 25 条用例一致；异常链、三个作用域、空 finally 消除、跨 try 的 return/break/continue 全对 |
| for-in / for-of | 同上 | **24** 条用例一致；迭代器关闭、穿越循环时的记录释放、跨层 break/continue 全对 |
| 对象与数组字面量 | 同上 | **55** 条用例一致；方法/取值器/计算键/`__proto__`/spread、数组三阶段与索引原子全对 |
| 赋值 | 同上 | 36 条用例一致；标识符/成员/计算键 × 普通/复合/逻辑三类，含"值是否被丢弃"的 `insert2` 差异 |
| 方法调用 | 同上 | 16 条用例一致；`get_field2`/`get_array_el2` + `call_method` 保住 `this` |
| `??` 与可选链 | 同上 | **20** 条用例一致；`?.` 短路整条链、`??` 不走真值跳转、同算符链共用 end 标签 |

这张表此前有 6 个数字是错的（旧值：35 / 55 / 39 / 41 / 365 / 14 / 20 / 56 / 21）。
它们是**写在文档里的常量，没有任何机制校验**，于是一路漂移。现在每个数字都能用
`./scripts/check.sh` 复现，上表由实测填充。

两处口径必须说清，否则数字会骗人：

- **"一致"只算真正比较过的用例。** `disasm_diff` 的 `compare()` 在参考实现自己也
  编译不过时会静默 `continue`，于是 3 条用例从未被比较（`[1,...,,]`、
  `continue` 到非迭代标签、`??` 与 `||` 混用）——三条都是非法输入，我们同样拒绝，
  结论不受影响，但上表按**实际比较数**写（所以是 55/20/24 而不是 56/21/24）。
  这类静默跳过本身就是隐患：若哪天我们开始接受它们，测试不会报错。
- **`constant_conditions_and_dead_jumps_match_the_reference` 是 `#[ignore]` 的**
  （缺 `fold_constants` 与 `resolve_labels`），单独跑会失败（21 条全比、全不一致）。
  它不计入上表。

### 上游语料的真实范围

旧文案写"`tests/` + `examples/` 全部 `.js` 一致"，实际是：

| 来源 | 总数 | 被测 | 未测原因 |
|---|---|---|---|
| `tests/*.js` | 15 | 8 | `assert.js`/`fixture_*` 是夹具；`microbench` 是基准；`test_intl`/`test_rw_handler`/`test_worker*` 依赖未实现的 Intl／std／Worker |
| `examples/*.js` | 6 | **全部 6** | —（原先只有 `hello_module.js` 被测，本轮补齐并修好两处差异） |

补齐 `examples/` 时抓到两个真实缺陷：

1. **`scriptArgs` 从未安装。** CLI 把位置参数存进 `opts.script_args`，赋值两处、
   读取零处，于是这个全局变量根本不存在。任何按
   `if (typeof scriptArgs != "undefined")` 取参数的脚本都会静默走进 fallback 分支：
   `examples/pi_bigint.js` 我们打印了一千位 π，参考打印 `usage: pi n_digits`。
2. **`.so` 导入的报错缺后缀。** 参考对原生模块报
   `could not load module filename '…' as shared library`，我们少了后半句——
   而这句正是"缺的不是文件、是 C 模块"的唯一提示。

仍有一处**已知的表层差异**：模块加载失败时参考多打一个空行（`…txt'\n\n`）。
运行期错误与语法错误都没有这个空行，它只出现在加载失败路径上；为此按消息字符串
嗅探不值得，记录在此。

### M2 尚未实现

以下语法都返回明确的 `SyntaxError`，不会静默生成错误字节码：
解构、类、生成器与 async（需要状态机）、`for await`、`with`、模块。

### 已知优化差距

有一个 `#[ignore]` 的差分测试列出了 15 个**语义正确但字节码不同**的用例，全部来自
参考实现的优化层，与本项目的功能无关：

- `fold_constants`：常量条件（`while(1)`、`if(0)`）应删除测试指令
- `skip_dead_code`：跳转之后的连续死跳转应删除
- `tail_call`：`return f()`、`return o.m()` 应编译为 `tail_call` / `tail_call_method`
- `get_length`：`a.length` 应改用专门的操作码

这些用例被单独列出而不是混在通过列表里，是为了让差距可见且不会悄悄扩大。

## 许可证

本项目采用 [MIT 许可证](LICENSE)。

上游归因：本项目是对 [bellard/quickjs](https://github.com/bellard/quickjs) 的 Rust 重写，
原实现同样以 MIT 许可发布，版权归 Fabrice Bellard 与 Charlie Gordon 所有。
`scripts/setup-oracle.sh` 会把上游源码下载到 `/tmp` 用作差分测试的对照实现，
该副本仅用于测试，不进入本仓库、也不参与构建。

## 提交约定

每个里程碑（M0、M1、…）单独提交，提交前必须 `./scripts/check.sh` 全绿；
提交信息用 `feat(module): 中文摘要 (Mx)`，正文写清验收证据（测试数、与 C 版对比结论）。

## 一致性测量

三个量具，回答三个不同的问题。都要求先跑一次 `./scripts/setup-oracle.sh`（构建 C 参考
实现；不构建时差分套件会打印 `SKIPPED`，也就是什么也没测）。

| 问题 | 命令 |
|---|---|
| 字节码／行为与参考实现是否一致 | `./scripts/check.sh`（内含 `scripts/lang-diff.py`） |
| README 公布的每个数字是否仍然成立 | `./scripts/check.sh`（内含 `scripts/verify-claims.py`） |
| 语言覆盖面有多少 | `TEST262_OUTCOMES=/tmp/now.txt ./scripts/test262.sh` |
| 这次改动是净修还是净退 | `python3 ./scripts/test262-diff.py /tmp/now.txt` |

第三个输出 `+N fixed / -M regressed`，按**用例名**求集合差而不是比总数——总数相同的
两次运行完全可能是一次回归加一次修好互相抵消。基线已入库
（`docs/test262-baseline.txt.gz`，23885 行逐用例结果），遇到任何回归脚本返回非零。

```sh
TEST262_GROUP=1 ./scripts/test262.sh        # 按部件看失败分布
TEST262_SIGNATURES=1 ./scripts/test262.sh   # 失败形态聚类
```

当前基线与已知缺陷见 [`docs/test262-status.md`](docs/test262-status.md)。
