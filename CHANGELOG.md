# 更新日志

本项目的版本号只表达**可用性边界**，不表达与上游 QuickJS 的功能对等度。

## 未发布

### 加：ES 模块（M6）—— 链接、循环导入、`import()`、JSON/text 模块

`qjs` 现在能跑模块：`import`/`export` 的四种形式、`export *`、`export * as ns`、
`import.meta`、动态 `import()`、`with { type: "json" | "text" | "bytes" }`、
`-m`/`--script` 与自动检测。三个验收物与参考**逐字节一致**：
`examples/hello_module.js`、`tests/test_cyclic_import.js`，以及一个 `a.js ↔ b.js`
互引且 `b` 在顶层调用 `a` 的函数的三文件用例。

实现上最值得记的一条：**模块的顶层绑定就是模块体的闭包变量**
（`ClosureType::ModuleDecl` / `ModuleImport`），读一个模块变量是普通的 `get_var_ref`，
所以 `is_global_slot` 里的 `ModuleDecl`/`ModuleImport` 分支必须去掉——它们在
v0.3.0 之前是"把模块变量当全局"的占位。剩下的全是"谁填这个 cell"：

* 自己的声明分配新 cell；
* import **直接用导出方的 cell**，不复制 —— live binding 与循环导入都是这一条的推论。

三个坑，都是对照参考才发现、且都会给出**看似合理**的结果：

1. **模块体要执行两次。** 编译出的前缀是 `push_this; if_false <body>`，于是
   以真值 `this` 调用时只建立提升的绑定、立刻 `return_undef`。这一次调用发生在
   **链接期**（`js_inner_module_linking` 末尾那次 `JS_Call(func, JS_TRUE)`），
   而不是求值期——正因如此，循环里任何模块的函数在每个函数体开跑前就已存在。
   先求值后提升，`b.js` 顶层调用 `a` 的函数就会看到 `undefined`。
2. **同一个文件作为 JSON 与作为 text 是两个模块。** 缓存键必须带上 attributes，
   否则 `import m from "./m.json"` 与 `import m from "./m.json" with { type: "text" }`
   会撞成一条记录，第二次导入静默拿到第一次的值——`hello_module.js` 正是这么写的。
3. **`declare_module` 恒返回 0**（它的调用者要的是 slot，而模块变量不是 slot），
   于是 `import * as ns` 的 `var_idx` 也成了 0，把前一个导入覆盖掉。

顺带修掉四个此前没被端到端测出来的解析器 bug：
* `export { a as b } from "m"` 被当成"导出本地绑定"检查，报 `exported variable 'a' does not exist`；
* `import("./x")` 在语句位置被当作 import 声明拒绝；
* `import()` 与 `import.meta` 此前没有编译路径；
* 自动检测规则换成了参考的规则：**首个 token** 是 `import`（后面不是 `.`/`(`）
  或 `export`，或后缀 `.mjs`。原先"脚本解析失败就当模块"会让只提到 `import.meta`
  的文件变成模块，而参考会按脚本处理并报错。

新增覆盖：`crates/quickjs-pure/tests/module.rs`（9 个用例，内存加载器，不碰文件系统）、
conformance 里整文件跑 `test_cyclic_import.js` + `hello_module.js` + 循环三文件用例。

### 已知差异（记录，不隐藏）

* **通用语法错误的输出格式不同。** 参考是 `SyntaxError: <消息>` 加一行
  `    at <file>:<line>:<col>`；我们是 `<file>: <line>: <col>: <消息>`，且列号有时偏向
  词法起点。与模块无关，早就存在，因为差分套件只比 stdout 与退出码所以一直没暴露。
  要修得让 `Error::Syntax` 带上位置而不是只带字符串。消息**文本**已对齐
  （例如 `import.meta only valid in module code`、`Could not find export 'x' in module 'm'`、
  `unsupported module type: 'x'` 都是参考的原文）。

## v0.3.0 — 2026-11-22

M5（ES6+ 运行期）完成。这一版的边界是：v0.2.0 发布时 M5 还没开始，
现在语言特性齐了——但模块系统（M6）与 `std`/`os`（M9）仍然缺。


### 加：`Promise`、async/await、`for await`、async generator

前两轮一直把 Promise 排除在外，理由写的是"对现有测试收益为 0"。那个判断错在
**测量方式**上：`test_language.js` 里有 30 个测试函数，其中一个
`async function test_await_using()` 是 `async` 声明的，而差分工具的切片正则只认
`^function `——它从未进入索引，于是"29/29 通过"实际上是 28 个测试的结果。
修好正则之后真相是 79/85，缺口正是那一个 async 用例。**M5 之前并没有完成，是靠
漏测通过的。**

这一版补齐了让那个用例真正跑起来所需的全部东西：

1. **`Promise`** —— 构造器、`then`/`catch`/`finally`、`resolve`/`reject`/
   `all`/`allSettled`/`any`/`race`/`try`/`withResolvers`、`[object Promise]`。
   状态是 `ObjectKind::Promise`；`resolve`/`reject` 与 `Proxy.revocable` 的
   `revoke` 一样做成 `Callable` 变体，因为 `NativeFn` 是函数指针、捕获不了它们要
   记住的那个 promise。
2. **微任务队列** —— 脚本执行完由 `run_script` 排空。队列里放的是**数据**而不是
   闭包，否则 GC 看不见；一个挂起的反应恰恰必须让它的事件处理器和 promise 活着。
3. **async/await** —— 完全复用生成器的协程机制：`await` 就是一次挂起，帧脱离后
   由微任务恢复。`call_value` 里那句"需要 Promise，尚未实现"的 `InternalError`
   换成了真正的执行。
4. **`for await`** —— 编译器不再拒绝；VM 补上 `for_await_of_next` 与
   `iterator_get_value_done`，因此既能迭代同步可迭代对象（回落到
   `Symbol.iterator`），也能迭代自定义的 `Symbol.asyncIterator`。
5. **async generator** —— `%AsyncGeneratorPrototype%`：`next`/`return`/`throw`
   返回 promise，`Symbol.asyncIterator` 指向自身。`yield v` 和收尾的 `return v`
   都会先 await，所以 `yield Promise.resolve(3)` 产出的是 `3`。
6. **未处理拒绝** —— 脚本结束后把没人接的拒绝按参考的格式报到 stderr 并以 1 退出：
   `Possibly unhandled promise rejection: <error>` + 栈。逐字节与参考一致。
7. **`AsyncDisposableStack.disposeAsync()`** 真的按顺序 await 每个 disposer（之前
   因为"没有 Promise"而同步跑完，顺序是错的）。

顺带修掉五处**既有** bug，都是这轮推进时才暴露出来的：

- **`async function` 体被生成了 `initial_yield`**：那是生成器的语义（创建时不跑
  体）。async 函数必须同步执行到第一个 `await`，否则 `log.push("a"); await 0`
  里的 `a` 永远不会在调用返回前出现——而且这种挂起与真正的 `await` 在运行期无法
  区分，只能在编译期就生成对。
- **`await` 后面多出 `if_false`/`return_async`**：参考实现的 `await` 只发一条
  指令。这一对是我们 VM 早期"value + magic"设计的补偿，`for await` 复用同一个
  `await` 时会多压一个值。现在 await 只压它的值，magic 只留给 `yield`。
- **`drive_async` 用"如何恢复"判断"如何结束"**：被 `throw` 恢复的体，如果
  `try { await p } catch {}` 之后正常返回，会被判成拒绝，拒绝理由是这个函数的
  返回值。
- **`AwaitResume` 多推一个微任务**：恢复被推迟一拍，导致 `f().then(g)` 跑到 `f`
  自己的下一句前面。现在在反应里直接恢复，顺序与参考逐字一致。
- **`async` 体的 `return promise` 没有采纳**：用 `fulfill` 而不是 `resolve`，
  于是 `async function f(){ return Promise.resolve(1) }` 交回的是"promise 的
  promise"。

### 加：`Proxy`

`Proxy` 和 `Proxy.revocable` 落地，十一个 trap 全部接线：`get`、`set`、`has`、
`deleteProperty`、`ownKeys`、`getOwnPropertyDescriptor`、`defineProperty`、
`getPrototypeOf`、`setPrototypeOf`、`isExtensible`、`preventExtensions`，外加
`apply`/`construct`（所以 `typeof proxyOfFunction === "function"`、
`new proxyOfConstructor()` 都对）。为此在 `ObjectKind` 上加了一个 `Proxy` 变体，
持有 target 与 handler 两个槽位；`revoke` 共享同一份槽位，清空即撤销，撤销之后
任何操作都是 `TypeError`。

不变量全部实现，这不是装饰 —— 没有它们，`new Proxy(frozen, {…})` 就能绕过
`Object.freeze`：非配置属性不许被藏、被删、被改名；非可扩展的 target 不许让
`isExtensible` 撒谎、不许让 `ownKeys` 多报或漏报、不许换原型；trap 存在但不可调用
是 `TypeError` 而不是"当作没有 trap"。

这一版还顺手补了三处**既有**的漏洞，都是"对象之外还藏着属性"这一类的：

1. **`Reflect.ownKeys([1,2])` 少了 `length`**。`length` 是数组的自有、非枚举、
   非配置属性，却存在元素数组里而不在属性表里，于是所有"取自有键"的视图都漏了它。
   这同时让 proxy 不变量覆盖不到 `length` —— handler 可以把 `length` 报成可配置的
   而没人拦。现在 `property_entries` 统一合成它。
2. **`Array.prototype.map.call(proxyOfArray, f)` 静默返回 `[]`**。一个数组的代理
   没有自己的元素数组，而所有高阶数组方法都从一个 `elements()` 读元素。现在
   `elements()` 走 `[[Get]]`，一次修好 `map`/`filter`/`reduce`/`join`/`slice`/
   `concat`/`flat` 等二十来个方法。同样地 `{...proxy}`、`Object.assign({}, proxy)`、
   `hasOwnProperty`、`JSON.stringify` 也都改走内部方法。
3. **`goto` 超过 127 字节被截断**。`choose_short_form` 无条件把 `Goto` 选成
   `Goto8`、`IfFalse` 选成 `IfFalse8`，不检查偏移是否装得下 i8。溢出后跳转落进
   字节码中间甚至函数之外，而 VM 把"跑出末尾"当作返回 —— 于是脚本**静默停止**，
   没有报错，也没有异常，看起来就像整个文件只有前半段。触发它需要恰到好处的形状：
   一个嵌套循环、循环体里四个含 `eval` 的闭包。现在短形式的选择是一个不动点：
   从最宽形式开始，只把**确实装得下**的跳转收窄，并且 `goto` 在装不下 i8 时退到
   `goto16` —— 与参考实现的指令选择一致（反汇编差分测试对着 oracle 通过）。

### 修：六个错答案，两个一直没被量到的测试文件

`test_loop.js` 和 `test_bigint.js` 是纯语言测试、不需要任何宿主注入，却从来没跑过。
补跑之后暴露出六个错误答案。它们都不是"缺功能"，而是**看起来合理、结果错**：

1. **`0n` 被当成真值**：`if (b & 1n)` 变成无条件分支。这是最恶劣的一处 ——
   它让每个按位判断的 BigInt 算法静默算错，`compute_pi(2000)` 从第 29 位开始
   全错（`…338951984927…` 而不是 `…338327950288…`），却照样打印出一串像 π 的数字。
2. **`Number == BigInt` 恒为真**：松散相等里那条分支丢了比较对象，拿右边的 BigInt
   和自己比，于是 `2 == 3n` 和 `0 == 7n` 都是 `true`。顺带修了精度：原本两边形如
   `100000000000000000001n == 1e20` 会因为都四舍五入成同一个 double 而判真，现在
   按精确整数比。
3. **BigInt 的 `++`/`--` 变成 `NaN`**：`ToNumeric` 保持类型，走数字路径就毁了。
   后果是所有 `for (i = 0n; i < n; i++)` 的 BigInt 算法只跑一轮。
   同时 `a.x++` 的返回值补上 `ToNumeric`（`a.x` 是 `true` 时旧值是 `1`，不是 `true`）。
4. **内层函数看不见后置的 `var`**：`function f(){ function g(){ return t } var t; }`
   里 `g` 读到的是**全局** `t`。原因是函数体在声明处就编译，那时 `var t` 还没声明。
   现在先提升完本作用域的所有名字，再编译函数体。
5. **`"a" + 1n` 抛 TypeError**：`+` 的拼串判断被排在 BigInt 算术之后。
6. **`Number(BigInt)` 是 `NaN`**，`BigInt("")` 是 `SyntaxError`：前者按 `ToNumber`
   取最近的 double，后者按 `StringToBigInt` 是 `0n`（而 `"+"`/`"-"` 仍是语法错）。

另外三处缺口补上：`for (a[0] in o)` 这类成员表达式循环目标（原先直接拒绝编译）、
`for (const [x] of o)` 这类解构循环目标（复用赋值模式的存储路径），以及
`for…in` 的枚举顺序 —— 整数键优先**只对单个对象成立**，原先被套用到整条原型链上，
于是一个原型上的 `"4"` 会插到自己的 `x`/`y` 前面；非枚举的自有属性现在也正确遮蔽
原型上的同名属性。

### 测试：差分套件终于进了门禁

`scripts/lang-diff.py` 把上游测试按函数逐个切片、逐个跑，结果与参考实现对比，并把
「已知缺口」写成清单：参考能过的测试我们必须过，清单里已经能过的一项会被报成
`STALE` 并让门禁失败，所以清单不会烂。参考自己也过不了的（`test_cyclic_labels` 会
死循环、WeakMap 系列要 `std`/`os`）单独归类忽略。`scripts/check.sh` 现在跑它；
参考实现没编译时**明确打印 SKIPPED**而不是静默跳过，CI 可用 `--require-oracle`
把它变成硬要求。

当前账（参考能跑的 80 个测试全过）：

| 文件 | 结果 |
|---|---|
| `test_language.js` | 30/30（含 `test_await_using`） |
| `test_closure.js` | 7/7 |
| `test_loop.js` | 18/18 |
| `test_bigint.js` | 5/5 |
| `test_builtin.js` | 20/20 |

已知缺口清单现在是**空的**。上游整文件语料（7 个脚本，含 `test_std.js`）的
stdout 与退出码也全部与参考一致。

## v0.2.0 — 2026-11-21

首次发布到 crates.io：`quickjs-gc`、`quickjs-parse`、`quickjs-compile`、`quickjs-pure`
（`quickjs-cli` 与 `quickjs-conformance` 标了 `publish = false` —— CLI 会跟 C 版 `qjs`
抢同一个命令名，conformance 只是 dev-dependency）。

**可用性边界**：`qjs` 能真正执行 JavaScript，并带标准库第一波。
`qjs -e 'print([1,2,3].map(x=>x*2).join())'` 打印 `2,4,6`。

### M4：标准库第一波

`Object`、`Function`、`Array`、`String`、`Number`、`Boolean`、`Math`、`JSON`、`Reflect`
与八个错误类，外加精确到 `file:line:column` 的栈回溯。

参考实现的 `test_builtin.js` 里 M4 范围的测试函数**全部通过**（14 个）：
`test`、`test_function`、`test_enum`、`test_array`、`test_array_extension`、
`test_array_iterator`、`test_string`、`test_math`、`test_number`、`test_json`、
`test_line_column_numbers`、`test_eval`、`test_eval2`、`test_date`。用
`scripts/slice-builtin-test.py` 可以复现（它把每个测试函数单独切出来，这样一个测试
失败不会因为整文件编译失败而掩盖其余；`test_eval` 依赖 `test_eval2`、
`test_array` 依赖 `test_array_extension`，切片时要把依赖一起保留）。

余下的 11 个不在 M4 范围，分属后面几个里程碑：`test_regexp`（M7）、`test_symbol`/
`test_typed_array`/`test_map`/`test_map1`/`test_weak_map`/`test_weak_map_cycles`/
`test_weak_ref`/`test_finalization_registry`（M8）、`test_generator`（M5）、
`test_rope`（M11：语义正确，10 万次拼接慢）。

### M4 收尾：`Date`、直接 `eval` 的作用域、spread 调用

**`Date`** —— 全部构造形式、`now`/`parse`/`UTC`、22 个 getter/setter、
六种字符串格式与 `toJSON`。两处不能照规范正文写，必须照参考实现逐句移植：

- `MakeTime`/`MakeDay` 的运算**顺序**是规范的一部分。参考实现用 `volatile`
  中间变量阻止编译器重排与 FMA 合并，`Date.UTC(1970, 0, 1, 80063993375, 29, 1,
  -288230376151711740)` 只有在那个顺序下才等于 `29312`。这里用同样的分步写法。
- **本地时区**由 `crates/quickjs-pure/src/tzif.rs` 直接读平台 TZif 数据库
  （`/etc/localtime` 或 `$TZ`），取 64 位转换块。参考实现叫 `localtime_r`；
  本 crate 不加外部依赖，所以自己解。`getTimezoneOffset` 按参考返回**整数**，
  否则 UTC 下会打印 `-0`。

**直接 `eval` 的作用域** —— eval 的代码现在能读写调用者的绑定，而不再只看全局：

- `this` 与 `arguments` 取自最内层调用者。
- 名字绑定沿**整个帧链**解析，所以内层函数里的 eval 能看到外层函数的局部变量；
  写回落到原来的槽位或闭包单元，因此 `f("a = 3")` 对调用者可见。
- strict 下 eval 有自己的变量环境：`eval("var v = 1")` 不再泄漏成全局，
  而对未声明名字的**赋值**仍然是 `ReferenceError` —— 声明与赋值是两条路。
- 已知边界：`let`/`const` 的暂时性死区、以及 eval 内声明的函数对调用者的可见性
  都还没有按规范细化。

**spread 调用参数** —— `f(...args)`、`o.m(...args)`、`new C(...args)` 编译为
「构造一个实参数组 + `apply`」。为此补了 VM 的 `apply`、`swap`、`perm3` 三条指令。
混合形式（`g(0, ...v, 3)`）走同一条路径，不需要额外特例。

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

### 栈回溯与位置信息

`error.stack` 现在可用，位置精确到 `file:line:column`，且与参考逐字一致。

- **编译期为每条指令记录位置**：`Function::positions` 是 `pc → (line, column)` 表，
  由 `pc2line` 一样的思路生成——只记位置变化的点，运行时二分查找。解析器把算符
  所在的 token 位置带到 AST（`Expr::op_pos`），因为出错的是**算符**而不是表达式
  起点：`1 + null[0]` 报 `[` 的列，`o.foo = 1` 报 `.` 的列，`not_def = 1` 报标识符
  的列，而 `a += poisoned` 报 `+=` 的列（读 `a` 成功、在 `ToPrimitive` 里才失败）。
- **`stack` 在错误被创建时生成**，不是被抛出时：`new Error("x").stack` 立刻可用，
  引擎内部抛的 `TypeError` 也带位置。`throw 42` 没有栈（非对象），参考同样如此。
- **`<input>` 与文件名的分工**：直接 `eval` 的代码叫 `<input>`，`-e` 叫 `<cmdline>`，
  脚本文件用自己的路径。宿主通过 `Context::set_script_name` 告诉引擎。
- **`JSON.parse` 的错误位置指向解析文本内部**：`JSON.parse('
 "  \@x"')` 的栈首行是
  `<input>:2:5`，其后才是调用者。
- **`Function.prototype.fileName`/`lineNumber`/`columnNumber`** 三个 getter 读函数自己的
  声明位置（参考实现私有约定，测试脚本依赖它）。

### 修：静默错误答案（不是"字节码不同"，是结果错）

跑参考的 `test_builtin.js` 时暴露出来的六处，每处都返回一个看起来合理但错误的答案：

1. **对象字面量的 getter 被 setter 覆盖**：`{get z(){…}, set z(v){…}}` 的第二个
   `define_method` 整体替换了属性，于是 `a.z` 是 `undefined`。现在只填自己那一半。
2. **`new` 不展开 `bind`**：`new (C.bind(null,1))()` 用 bound 的 `this` 跑函数体，
   报 `cannot set property 'x' of null`。现在解包到 target、按 target 的原型造对象。
3. **算术运算符不走 `ToPrimitive`**：`2 * {valueOf(){return 3}}` 是 `NaN`，且
   `valueOf` 抛的异常被吞掉。现在走 `ToNumber`。
4. **访问器函数没有名字**：`{get z(){}}.z.name` 是空串，栈帧因此打印成 `at  (…)`。
   现在叫 `get z`/`set z`（`set_name` 不能把空串当成"已有名字"）。
5. **`"use strict"` 泄漏到函数的兄弟**：解析器保存/恢复了 `ctx` 与 `top_level`，
   却漏了 `strict`，于是某个函数里的指令让整个脚本变 strict，**它后面**的函数声明
   失去声明身份，调用时报未定义。
6. **先引用、后声明的全局函数解析错**：`function a(){ b() } function b(){}` 在编译
   `a` 时还不知道 `b`，把这个名字记成了未声明的全局，等真正遇到声明时直接 return，
   于是 `b` 在运行期不存在。现在会把那个猜测改成声明。

另外补齐了：

- **`toString(radix)` 的精确转换**：新增 `bignum` 模块（只用加、减、乘、移位、
  除以小整数和按基数长除），按参考 `dtoa.c` 的算法找「能读回同一个 double 的最短
  数字串」。旧的浮点近似会少一位或末位舍错（`(1.3).toString(7)` 是
  `1.204620462046204621`，参考是 `1.2046204620462046205`）。
- **`delete <名字>`**：新增 `delete_var` 指令。声明的绑定（局部、参数、全局声明）
  一律 `false`；**隐式全局**（`x = 1` 且无声明）是全局对象上可配置的属性，真的删得掉。
- **strict 模式下给未声明名字赋值抛 `ReferenceError`**：之前静默创建全局。
- **直接 `eval` 继承调用者的 strictness**：`"use strict"` 函数里的
  `eval("x = 1")` 现在抛 `ReferenceError`。

### 已知缺口（记录，不隐藏）

- `RegExp`、`Symbol`、`Map`/`Set`、TypedArray、生成器/async。
- **`Function.prototype.toString`** 返回 `[native code]` 而不是源码，需要保留源码
  与函数自身的 span。
- **字符串拼接是 O(n²)**：`test_rope` 的 10 万次拼接要几十秒（参考有 rope）。
  属 M11 性能优化。
- `Object.freeze` 后的数组在参考里退化为稀疏形态、打印成 `[ 0: 1, … ]`；
  这是其内部表示泄漏，未复刻。

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
