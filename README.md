# quickjs-rs

用**纯 Rust** 重写 [bellard/quickjs](https://github.com/bellard/quickjs)（基线 `VERSION = 2026-06-04`），
目标：不依赖 C 代码即可构建，功能/语义等价，可替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译）。

- 📄 需求分析：[`docs/requirements.md`](docs/requirements.md)（v0.5，关键决策已确认）
- 📌 当前版本：**v0.2.0** —— 首个发布版本：`qjs` 能执行 JS，标准库第一波可用
- 🚧 当前进度：**M0–M4 已完成**；M5（ES6+ 运行期）未开始
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
| M5–M11 | ES6+ 运行期、模块、正则/Unicode、BigInt/TypedArray、std/os/Worker、AOT、性能 | ⬜ |
| M12–M14 | C ABI 层、Temporal、Intl | ⬜ |

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
**已发布到 crates.io（v0.2.0）**：`quickjs-pure` 是门面 crate（lib 名仍是 `quickjs`），
`quickjs-gc`/`quickjs-parse`/`quickjs-compile` 是其依赖。`quickjs-cli` 与
`quickjs-conformance` 标了 `publish = false`，分别因为会跟 C 版 `qjs` 抢命令名、以及
只是 dev-dependency。用法：

```toml
[dependencies]
quickjs = { package = "quickjs-pure", version = "0.2" }
```

M3（能执行 JS）与 M4（标准库第一波）都已达成：
`Object`、`Function`、`Array`、`String`、`Number`、`Boolean`、`Math`、`JSON`、
`Reflect` 与错误类均可用，`qjs` 能跑通参考实现的 `test_builtin.js` 中 M4 覆盖的部分。
`error.stack` 可用：位置来自编译期为每条指令记录的表，所以栈帧能报到
`file:line:column`，抛错、`catch`、`JSON.parse` 的语法错误都带它。
剩下的已知缺口（`Date`、`RegExp`、`Symbol`、`Map`/`Set`、TypedArray、生成器/async、
spread 调用参数、直接 `eval` 的作用域、`Function.prototype.toString` 的源码回读、
字符串拼接的 rope 优化）记在 `docs/requirements.md`，发布计划见同处的「发布策略」。

## 开发

```sh
./scripts/setup-oracle.sh     # 一次性：构建 C 参考实现到 /tmp/quickjs-master
./scripts/check.sh            # fmt + clippy + test（提交前必跑）
cargo run -p qjs -- --help    # 与上游 qjs --help 逐字节一致
```

差分测试会自动寻找 `$QJS_ORACLE`、`/tmp/quickjs-master/qjs` 等路径；找不到参考实现时
相关用例打印 `skipping:` 而不是失败。引擎尚未能执行 JS 时，`qjs` 以退出码 2 + stderr
`not implemented yet` 表示“待实现”，harness 记为 *pending* 而非失败。

## 与 C 版的一致性验证

| 层面 | 差分手段 | 结果 |
|---|---|---|
| CLI | `qjs --help` / 用法错误 | stdout+stderr+退出码**逐字节一致** |
| 数值字面量 | `DataView` 取 f64 两半 | 35 个字面量**逐位相同** |
| 字符串字面量 | `JSON.stringify` 往返 | 13 个字面量解码一致 |
| 语法接受性 | `qjsc -c`（编译不执行）退出码 | 104 有效 + 55 无效 + 39 模块用例一致 |
| 声明冲突 | 同上 | 41 个作用域冲突用例一致 |
| 上游语料 | 同上 | `tests/` + `examples/` 全部 `.js` 一致 |
| 字节码 | `qjsc-dump`（`-DDUMP_BYTECODE=1`）逐 opcode 反汇编 | 365 条用例逐字节一致 |
| TDZ/作用域 | 同上 + 14 条不变量测试 | 作用域链、check 操作码、捕获与关闭全部对齐 |
| break/continue/标签 | 同上 | 21 条用例一致；跳转目标栈、多层标签、do-while 的 test 与 for 的 update 全对 |
| switch | 同上 | 18 条用例一致；fallthrough、`default` 任意位置、空 `case` 链全对 |
| try/catch/finally | 同上 | 25 条用例一致；异常链、三个作用域、空 finally 消除、跨 try 的 return/break/continue 全对 |
| for-in / for-of | 同上 | 20 条用例一致；迭代器关闭、穿越循环时的记录释放、跨层 break/continue 全对 |
| 对象与数组字面量 | 同上 | 56 条用例一致；方法/取值器/计算键/`__proto__`/spread、数组三阶段与索引原子全对 |
| 赋值 | 同上 | 36 条用例一致；标识符/成员/计算键 × 普通/复合/逻辑三类，含"值是否被丢弃"的 `insert2` 差异 |
| 方法调用 | 同上 | 16 条用例一致；`get_field2`/`get_array_el2` + `call_method` 保住 `this` |
| `??` 与可选链 | 同上 | 21 条用例一致；`?.` 短路整条链、`??` 不走真值跳转、同算符链共用 end 标签 |

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
