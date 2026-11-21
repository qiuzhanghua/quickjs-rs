# quickjs-rs

用**纯 Rust** 重写 [bellard/quickjs](https://github.com/bellard/quickjs)（基线 `VERSION = 2026-06-04`），
目标：不依赖 C 代码即可构建，功能/语义等价，可替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译）。

- 📄 需求分析：[`docs/requirements.md`](docs/requirements.md)（v0.5，关键决策已确认）
- 📌 当前版本：**v0.1.0** —— 解析器与字节码编译器可用，**运行期尚未开始**（`qjs` 还不能执行 JS）
- 🚧 当前进度：**M2 进行中**（字节码编译器核心已通过逐 opcode 差分），M0/M1 已完成
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
| M0 | 工作区骨架、`js-gc`（Gc/WeakGc/循环回收）、差分测试框架、`qjs` 选项面 | ✅ |
| M1a | 词法分析器（ES2025 全量词法） | ✅ |
| M1b | AST + 解析器 + early errors + 作用域分析 | ✅ |
| M2 | 字节码编译器 + 作用域解析 + 序列化 | 🚧 核心子集已通过差分（见下） |
| M3 | VM 核心（调用/对象/异常/闭包/原型） | ⬜ |
| M4–M11 | 内建库、ES6+ 运行期、模块、正则/Unicode、BigInt/TypedArray、std/os/Worker、AOT、性能 | ⬜ |
| M12–M14 | C ABI 层、Temporal、Intl | ⬜ |

## 已交付的结构

```
crates/js-gc/                引用计数 + 循环回收（16 个测试）
crates/js-parse/             词法 + AST + 解析器（34 单元 + 36 结构 + 差分：字面量/语法/作用域/上游语料）
crates/js-compile/           字节码编译器：opcode 表（转录自上游头文件）+ 两阶段编译 + 反汇编 + 序列化
crates/quickjs/              公共 API 骨架：Runtime / Context / Error / COMPAT_VERSION
crates/qjs/                  CLI：选项面与上游逐字节一致（--help、用法错误、-q）
crates/quickjs-conformance/  差分测试 harness（候选 vs C 参考实现）
scripts/setup-oracle.sh      下载并构建 C 参考实现（oracle）
scripts/check.sh             提交前的门禁：fmt + clippy -D warnings + test
```

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
| 字节码 | `qjsc-dump`（`-DDUMP_BYTECODE=1`）逐 opcode 反汇编 | 138 条用例逐字节一致 |
| TDZ/作用域 | 同上 + 14 条不变量测试 | 作用域链、check 操作码、捕获与关闭全部对齐 |
| break/continue/标签 | 同上 | 24 条用例一致；跳转目标栈、多层标签、do-while 的 test 与 for 的 update 全对 |
| switch | 同上 | 20 条用例一致；fallthrough、`default` 任意位置、空 `case` 链全对 |
| try/catch/finally | 同上 | 25 条用例一致；异常链、三个作用域、空 finally 消除、跨 try 的 return/break/continue 全对 |

### M2 已知差距

编译器目前覆盖：字面量、算术/比较/逻辑/位运算、变量读写（含 `var`/`let`/`const` 的全局与局部差异）、
`if`/`while`/`do`/`for`、函数声明与表达式、闭包、调用、成员访问、`return`、`typeof`/`void`/`!`/`~`、
自增自减、复合赋值、条件与逗号表达式。反汇编与参考实现**逐字节一致**。

当前已覆盖 `break`/`continue` 与标签（含多层标签链、跨层 `break outer`）、`switch`
（fallthrough、`default` 写在任意位置、空 `case` 链、`break`）、`try`/`catch`/`finally`
（异常边、`gosub`/`ret` 共享 finally、catch 参数与三个独立作用域、跨 try 边界的
`return`/`break`/`continue` 逐层展开 finally），以及 TDZ 与作用域链：`let`/`const` 在作用域入口被标记未初始化（`set_loc_uninitialized`），
读取走 `get_loc_check`、写入走 `put_loc_check`/`set_loc_check`，捕获的绑定分配引用格并在
离开块时 `close_loc`，函数声明在作用域入口实例化，`global` 访问通过 `global_ref` 逐层转发，
`return` 之后的死代码不编译。

尚未实现（都返回明确的 `SyntaxError`，不会静默生成错误字节码）：
`for-in`/`for-of`、解构、类、生成器与 async（需要状态机）、
`with`、模块、可选链、数组字面量与对象字面量、`??`。



### 已知优化差距

有一个 `#[ignore]` 的差分测试列出了 15 个**语义正确但字节码不同**的用例，全部来自
参考实现的优化层，与本项目的功能无关：

- `fold_constants`：常量条件（`while(1)`、`if(0)`）应删除测试指令
- `skip_dead_code`：跳转之后的连续死跳转应删除
- `tail_call`：`return f()` 应编译为尾调用

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
