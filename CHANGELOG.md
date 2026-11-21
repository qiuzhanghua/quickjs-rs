# 更新日志

本项目的版本号只表达**可用性边界**，不表达与上游 QuickJS 的功能对等度。

## 未发布

### M4：标准库第一波

`Object`、`Function`、`Array`、`String`、`Number`、`Boolean`、`Math`、`JSON`、`Reflect`
与八个错误类。参考实现的 `test_builtin.js` 中 M4 覆盖的 237 条语句通过 229 条。

几处容易写错、都按参考行为实现的语义：

- **`Object.prototype.toString`** 用标签判断（`[object Array]`），不是查类字段。
- **`Object.is`** 与 `===` 只差两处：`NaN` 与 `-0`。
- **`defineProperty`** 省略的属性描述符一律为 `false`，所以能造出不可见属性。
- **`freeze`/`seal`/`preventExtensions`** 三者可区分（`isFrozen`/`isSealed`），
  且失败写入在 sloppy 下静默、strict 下 `TypeError`；`sort`/`reverse` 这类写回方法
  一律抛错，因为它们走 `[[Set]]`。
- **数组的洞是真洞**：`[1,,3]` 与 `[1,undefined,3]` 不同，`forEach` 跳过前者，
  打印器显示 `<1 empty item>`；`a.length = 5` 用洞填充，`a[3]=4` 在中间留洞。
- **`Math.round`** 半数向 `+∞` 舍入（`-0.5` → `-0`），**`toFixed`/`toExponential`/
  `toPrecision`** 半数远离零（Rust 的格式化是半数取偶，`(1.125).toFixed(2)` 会差一位）。
- **`Math.sumPrecise`** 不是补偿求和：它要「精确求和后一次舍入」，三累加器才够。
- **`sort` 默认按字符串序**，所以 `[10,9,1].sort()` 是 `[1,10,9]`。
- **数组方法泛型**：`Array.prototype.join.call("abc")` 是 `a,b,c`，而 `sort.call("aa")`
  抛 `TypeError`（字符串盒的索引只读）。
- **数组迭代器惰性**：`keys.call({length: 2**32+1})` 只推进几步，不能物化。
- **`arguments`** 由 `special_object` 在函数体开头初始化，是普通对象而非数组。
- **sloppy 模式的 `this`** 会把原始类型装箱（`typeof this` 是 `object`），strict 不装箱。
- **`bind`** 用独立的 `Callable::Bound` 变体承载 target/this/leading args，
  而不是塞进属性（那些不可观测）。

### 修：M4 过程中暴露的编译器与词法 bug

1. **词法层不合并代理对**：`"\u{dbff}\u{dfff}"` 与 `"\u{10ffff}"` 曾是两个字符串。
   Rust 的 `char` 存不了孤立代理项，但**成对**的两个转义必须合成一个码点；
   状态改为词法器字段，跨转义保留高半。
2. **`put`+`get` 折叠在标签之后仍然发生**：`for (i=0; …)` 的 init 多留一个值
   （`dup` + `drop`），后置折叠把 `dup;put` 变 `set` 再吃掉 `drop`，于是循环条件
   比的是上一次迭代的残留。现在标签用 `label_since_insn` 标志记录，init/update
   的表达式改用已有的 discard 机制，与参考形状一致。
3. **`get_length`** 补上：`.length` 读取有自己的 opcode，参考对任何对象都用它。
4. **`Loc8` 窄槽编码**：`encode_operands` 对 `Loc8`/`Loc16` 静默不写操作数（同 M2
   的 `AtomU8` 一类问题），`inc_loc` 因此编码出垃圾字节。
5. **`compile_function` 早退留下 `FnState`**：后续 `stmt_depth -= 1` 作用到错的状态。

### 已知缺口（记录，不隐藏）

- `Error.stack` / 栈回溯：需要 `pc2line` 表，`assert_json_error` 因此仍失败。
- `toString(radix)` 的**小数**部分精度不足：`(1.3).toString(7)` 少两位，
  需要精确 bignum 转换。
- `Object.freeze` 后的数组在参考里退化为稀疏形态、打印成 `[ 0: 1, … ]`；
  这是其内部表示泄漏，未复刻。
- `Date`、`RegExp`、`Symbol`、`Map`/`Set`、生成器/async、直接 `eval` 的作用域。

### M3：VM 核心

`qjs` 现在真的执行 JavaScript：`qjs -e 'print(1+1)'` 打印 `2`，
`function fib(n){…} print(fib(20))` 打印 `6765`。

- **值层** —— `Value` 的 `Int`/`Float` 之分镜像参考实现的 `JS_TAG_INT` /
  `JS_TAG_FLOAT64`；`ToNumber`/`ToInt32`/`ToString`/`ToBoolean`/`==`/`===`/`typeof`
  按规范实现，含 `-0`、`NaN`、`"12px"` 不是数字这些细节。数字打印的阈值与指数写法
  与参考逐字对齐（`1e-7`、`1e+21`、`0.30000000000000004`）。
- **对象模型** —— 属性表（保留插入顺序，整型键优先升序）、数组元素向量、原型链、
  访问器、函数对象。引用计数 + 循环回收直接复用 `quickjs-gc`：VM 栈上的 `Gc` 句柄
  天然是根。
- **解释器** —— 帧、操作数栈、116 个 opcode 中的执行面。异常不用旁表：`catch`
  往值栈压一个标记，抛出时向下扫找到最内层的那个，这就是参考实现的做法。
- **调用与闭包** —— 捕获变量放进堆上的 cell，`var_ref_count` 在入帧时分配；
  被捕获的**实参**必须从实参槽搬进 cell，否则闭包读到的是空值。
- **构造** —— `new C(args)` 走 `call_constructor`；函数对象带 `prototype`，其
  `constructor` 指回去（一个 GC 环，正是循环回收存在的理由）。
- **内建** —— 仅 `print` / `console` / `undefined` / `NaN` / `Infinity`。
  完整的 `Object`/`Array`/`String`/`JSON` 属 M4。
- **`new`** —— 编译器与 VM 两侧补齐（`callee; dup; args…; call_constructor n`）。
- **箭头函数的简写体** —— `x => x*2` 此前报 "concision bodies are not supported"。

### 修：VM 跑起来之后暴露的五个编译器 bug

这些都不是"字节码不同"，而是**语义错**，差分套件覆盖不到，只有真跑才看得见：

1. **`put`+`get` 折叠穿透了标签** —— `for (var i=0; i<3; i++)` 的初始化与循环头之间
   有一个跳转目标，把 `put_loc i; get_loc i` 折成 `set_loc i` 会让条件比较上一轮残留
   在栈上的值。现在标签在中间就不折叠。
2. **`-0` 被折掉了符号** —— `-0` 常量折成 `push_0`，于是 `1/-0` 变成 `Infinity`。
   参考只折叠负**整数**，`-0` 与 `-2.5` 都保留 `neg`。
3. **`var` in `for` 头变成局部变量** —— 全局性判断看的是"游标是否在 body 块"，
   而 `var` 属于函数作用域。现在按声明种类判。
4. **脚本 body 块键与 offset 0 撞车** —— 顶层 `{ let x }`、`for (let i…)` 因此丢掉
   自己的作用域（`let` 变成全局）。body 块改用语句不可能产生的键。
5. **`<ret>` 槽只 push 了 `vars` 没 push `var_block`** —— 索引从此错位一格，
   脚本自身块的 `set_loc_uninitialized`（TDZ 标记）全部丢失。函数帧没有 `<ret>`，
   所以只在顶层暴露。

另外修掉两处**状态泄漏**：`compile_function` 早退时把 `FnState` 留在栈上（后续
`stmt_depth -= 1` 于是作用到错的状态，表现为"subtract with overflow"崩溃）。

### 赋值、`??` 与可选链

- **语法错误修复（三处静默错误码）** —— 这三个都不是"字节码不同"，而是编译出来
  的字节码**语义是错的**，而差分套件里一条赋值用例都没有，所以一直没被发现：
  - `o.a += b` 编译成了 `o.a = b`：复合赋值的成员/计算键路径根本没读旧值，
    操作符被 `Nop` 吞掉。现在用 `get_field2`（`get_array_el3`）把对象（与键）
    留在栈上再读值。
  - `a ||= b` / `a &&= b` / `a ??= b` 编译成了 `a = b`：无条件求值右操作数并
    永远赋值。现在先 `dup` + 测试 + 跳到末尾，右操作数只在需要时才求值。
  - `o.m(args)` 丢掉了 `this`：`get_field` + `call` 没有接收者槽位，调用时
    `this` 是空的。现在改为 `get_field2`（`get_array_el2`）+ `call_method`。
    注意方法调用与复合赋值用的是不同的"读"：前者键用完即弃，后者要留键回写。
- **`??`** —— `is_undefined_or_null` + `if_false`。不能复用真值跳转：
  `0 ?? b` 是 `0`，而 `0 || b` 是 `b`。
- **可选链** —— `?.` 短路的是**整条链**而不是自己那一节：`a?.b.c` 在 `a` 为
  nullish 时不能去读 `undefined.c`。解析器已经把链的范围包成
  `ExprKind::OptionalChain`，编译器只在这层建一个 end 标签，链上每个 guard 都
  跳过去。
- **同算符逻辑链共用 end 标签** —— `a || b || c` 此前每节一个标签，第一个测试
  命中后落进第二节的测试里重新判一次（语义不变但多绕）。现在按左脊展开成操作数
  序列，一次测试直接跳到末尾；`(a || b) && c` 因为算符不同仍各用各的标签。
- **赋值是否留下值** —— `put_field`/`put_array_el` 会吃掉对象和值，所以"值被保留"
  的赋值才需要 `insert2`/`insert3`。此前无条件加，导致表达式语句多留一个栈槽；
  现在由上下文决定。脚本里的表达式语句例外：它的值就是程序的值。
- **逻辑型成员赋值漏了 store 路径的 `goto`** —— keep 路径要靠 `nip` 丢掉保留的
  对象，store 路径必须跳过它；少了这条 `goto` 会直接落进 `nip`，而那时栈上只有
  值，等于下溢。

### 对象与数组字面量

- **对象字面量** — `define_field` / `define_array_el` / `define_method` /
  `define_method_computed` / `set_proto` / `copy_data_properties`。指令取决于属性
  **怎么写**而不是值是什么：`{a: 1}` 是 `define_field`，`{a(){}}` 是 `define_method`，
  `{__proto__: p}` 是 `set_proto`（`{__proto__}` 简写不是），计算键先用
  `to_propkey` 而计算方法不用（`define_method_computed` 自己转换）。
- **数组字面量** — 参考实现的三阶段：前 32 个连续普通元素走 `array_from`；遇到洞
  或前 32 个之后改用显式索引 `define_field <i>`；遇到 spread 后索引上栈，用
  `append` / `define_array_el` / `inc`。洞会让 `length` 需要单独修补，因为显式索引
  不会像压入元素那样延长数组。

### 修正

- **AST 增加 `PropertyKind`**：`MethodKind::Method` 原先同时表示 `{m(){}}` 和
  `{a: 1}`，编译器无法区分二者，而它们编译出的指令不同。现在对象字面量用独立的
  `PropertyKind::{Value, Method, Get, Set}` —— `MethodKind::Constructor` 本来也不可能
  出现在对象字面量里，共用这个枚举本身就是类型在说谎。顺带修掉了
  `expr_to_pattern_inner` 里一个恒真的判断（四个分支全过，等于没有判断）。
- **`Atom` 区分字符串与数组索引**：参考实现把「规范数组索引」（无符号、无前导零、
  ≤ 2^31−1）做成 tagged int 原子而不是字符串，所以 `{2: x}` 反汇编成
  `define_field 2`，而 `{1.5: x}` 是 `define_field "1.5"`。现在 `Function::atoms` 是
  `Vec<Atom>`，序列化加了种类标签（字节码格式版本 2 → 3），索引原子按数字往返。
- **`encode_operands` 补全 `AtomU8`/`AtomU16`/`AtomLabelU8`/`AtomLabelU16`/`LabelU16`/
  `NpopU16`**。此前这些格式落到 `_ => {}`，静默不写任何操作数 —— 写出来的字节码看着
  合理却是另一条指令。这一条是 `define_method` 暴露出来的：没有它，`AtomU8` 的原子
  和标志位一个都不会被编码。

### 测试

233 个测试，30 个测试套件，全部通过；`cargo clippy -D warnings` 干净。

VM 侧新增 `tests/run.rs`：17 条行为断言，期望值全部取自参考实现（`qjs -e …`）而不是
我们自己的输出。另有 36 条 `qjs -e` 端到端差分全对。

字节码逐 opcode 差分增至 **365 条**用例（新增对象与数组字面量 56 条、赋值 36 条、
方法调用 16 条、`??`/可选链 21 条），另有 11 条字面量与 9 条赋值结构不变量测试。

赋值这条补齐的意义大于数字：成员赋值此前在差分套件里**一条用例都没有**，正是
三处静默错误码长期存活的原因。

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
