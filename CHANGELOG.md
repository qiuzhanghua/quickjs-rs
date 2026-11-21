# 更新日志

本项目的版本号只表达**可用性边界**，不表达与上游 QuickJS 的功能对等度。

## 未发布

### 包名与发布准备（行为无变化）

crates.io 上 `quickjs`、`qjs`、`quickjs-rs` 三个名字都已被他人占用，其中
`quickjs`（2019，FFI 包装）与 `qjs`（2019，`qjs-sys` 绑定）都还很活跃；
`quickjs-rs` 的版本虽已全部 yank，yank 不等于让名。因此包名一律加前缀，
**lib 名与二进制名保持不变** —— 包名与模块路径是两件事。

| 目录 | 包名 | lib / bin |
|---|---|---|
| `crates/quickjs-gc` | `quickjs-gc` | lib `js_gc` |
| `crates/quickjs-parse` | `quickjs-parse` | lib `js_parse` |
| `crates/quickjs-compile` | `quickjs-compile` | lib `js_compile` |
| `crates/quickjs-pure` | `quickjs-pure` | lib `quickjs` |
| `crates/quickjs-cli` | `quickjs-cli` | bin `qjs` |
| `crates/quickjs-conformance` | `quickjs-conformance` | lib，不发布 |

配套改动：

- 内部依赖同时写 `path` 与 `version`（集中在 `[workspace.dependencies]`，
  version 用宽版本 `"0.1"`，patch 升级只改 `workspace.package.version`），
  这是 `cargo publish` 的硬要求：发布时 `path` 会被去掉，只留版本约束。
- 补 `repository`、`readme`、`keywords`、`categories`；`readme` 从 workspace
  继承根 `README.md`，每个待发布 crate 目录内放 `LICENSE` 副本。
  此前 `cargo package` 会警告「没有 documentation/homepage/repository」，
  打包产物里也没有 README 与 LICENSE。
- `quickjs-cli` 与 `quickjs-conformance` 设 `publish = false`：前者会和 C 版抢
  同一个 `qjs` 命令名，后者依赖本机构建的 C 参考实现。

**发布本身仍未做**，本项目在 M3 让 `qjs` 能真正执行 JavaScript 之前不发布 ——
理由记在 `docs/requirements.md` 的「发布策略」。当前 `cargo package` 在
`quickjs-pure` / `quickjs-compile` 上仍会离线失败，原因是本机 cargo 走 mirror
索引且这两个包的上游依赖尚未上传；只要 path 依赖都带了 version，发布顺序
`quickjs-gc → quickjs-parse → quickjs-compile → quickjs-pure` 即可。

## v0.1.1 — 2026-11-21

**可用性边界未变**：`qjs` 仍不能执行 JavaScript，任何需要求值的命令依旧以
`not implemented yet` 退出。这个版本扩大的是一条已经跑通的编译链路的覆盖面，
不是可用性 —— 按本项目的版本规则，这正是不该抬到 `0.2.0` 的理由。

抬到 `0.2.0` 的时刻是 **M3 让 `qjs` 跑起来**。

### 编译器新增

- **`try`/`catch`/`finally`** — 异常表与 `catch_offset`、`gosub`/`ret` 让三个出口
  共享一份 finalizer、空 `finally` 被消除、catch 参数与 block/handler 三个独立作用域。
- **跨 `try` 边界的 `return`/`break`/`continue`** — 逐层展开 finally，`nip_catch`
  与 `gosub` 的顺序、以及「值已在栈上」时不再补 `undefined`。
- **`for`/`in` 与 `for`/`of`** — 枚举记录随循环留在值栈上；`for`/`of` 在退出时
  `iterator_close`，`for`/`in` 只 `drop`。`break` 落到唯一的清理指令上，因此循环内的
  `break` 自身不发指令，只有*穿越*整个循环的跳转会为它的记录付费。`return` 出
  `for`/`of` 时按 `nip_catch` → `rot3r` → `undefined` → `iterator_close` 收尾。

### 修正

- 循环变量的绑定 opcode 区分「初始化」与「赋值」：声明式用不带检查的 `put_loc`
  （这正是把 `let` 带出 TDZ 的动作），赋给已有绑定才用 `put_loc_check`。
- 循环变量绑定与循环体之间保留参考实现的 `label_body` 标签。少了它，
  `put_loc v; get_loc v` 会被折叠成 `set_loc v`，循环体首个读取消失。

### 测试

155 个测试，23 个测试套件，全部通过；`cargo clippy -D warnings` 干净。

字节码逐 opcode 差分从 138 条用例增至 **236 条**，新增的部分全部逐字节一致：
`try/catch/finally` 25 条、`for`/`in`/`of` 20 条，以及跨 try 转移与标签组合。

## v0.1.0 — 2026-06-04

第一个版本：**解析器与字节码编译器可用，运行期尚未开始**。

`qjs` 仍不能执行 JavaScript —— 任何需要求值的命令都会以 `not implemented yet`
退出。这个版本对标的是一条已验证的编译链路，不是一台能用的引擎。

### 可用的部分

- **`js-gc`** — 引用计数 + trial-deletion 循环回收，`Gc`/`WeakGc`，确定性释放。
- **`js-parse`** — ES2025 全量词法分析器，AST，完整解析器，early errors，
  作用域分析。与 C 参考实现做差分，覆盖字面量、语法、作用域与上游语料。
- **`js-compile`** — 字节码编译器。反汇编输出与参考实现**逐字节一致**，覆盖：
  - 字面量、算术/比较/逻辑/位运算、变量读写（`var`/`let`/`const` 的全局与局部差异）
  - `if`/`while`/`do`/`for`、`break`/`continue` 与标签（多层标签链、跨层 `break outer`）
  - `switch`（fallthrough、`default` 在任意位置、空 `case` 链）
  - 函数声明与表达式、闭包、调用、成员访问、`return`
  - TDZ 与作用域链：`set_loc_uninitialized`/`get_loc_check`/`put_loc_check`，
    捕获绑定分配引用格并在离开块时关闭
  - `typeof`/`void`/`!`/`~`、自增自减、复合赋值、条件与逗号表达式
- **`qjs`** — 选项面与上游逐字节一致（`--help` 文本、用法错误、退出码、组合短选项）。
- **`quickjs-conformance`** — 候选实现与 C 参考实现之间的差分测试 harness。

143 个测试，23 个测试套件，全部通过；`cargo clippy -D warnings` 干净。

### 尚未实现

运行期整体缺失（M3+）：对象模型、调用栈、异常、内建库、模块、正则、BigInt、
TypedArray、`std`/`os`、AOT、C ABI 层、Temporal、Intl。

编译器层面仍未实现，均返回明确的 `SyntaxError` 而不是静默生成错误的字节码：
`try/catch/finally`、`for-in`/`for-of`、解构、类、生成器与 async、`with`、模块、
可选链、数组与对象字面量、`??`。

### 已知优化差距

以下用例**语义正确但字节码不同**，全部来自参考实现的优化层，单独记录在一个
`#[ignore]` 的差分测试里，让差距可见且无法悄悄扩大：

- `fold_constants` — 常量条件（`while(1)`、`if(0)`）应删除测试指令
- `skip_dead_code` — 跳转之后的连续死跳转应删除
- `tail_call` — `return f()` 应编译为尾调用

### 许可

MIT。这是对 [bellard/quickjs](https://github.com/bellard/quickjs) 的重写，
原实现同为 MIT 许可。
