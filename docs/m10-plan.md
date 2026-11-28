# M10 拆解：语言收尾（私有类元素、早期错误收口），以及为什么 AOT 顺延

状态：**待开工**。本文件的两条结论都来自测量，不是来自里程碑清单的下一行。
验收基准：参考实现 `oracle/quickjs-master/qjs`（`VERSION = 2026-06-04`）+ test262 逐用例差分。

---

## 0. 结论先说

M9 之后"能不能被嵌入使用"已经不再是问题：`std`/`os`/事件循环/REPL 都在，`qjs --std` 的
探针脚本与参考逐字节一致。但引擎现在**跑不动现代 JS 库**，因为私有类字段编不过——而这不是
一个小众语法：`language` 8599 条失败里 **3451 条（40%）** 是它一个人。

所以下一步不是清单上的 AOT，是语言收尾。顺序与理由：

| 步 | 内容 | 可量的收益 | 为什么排在这 |
|---|---|---|---|
| **M10a** | 私有类元素（`#x` 字段/方法/访问器/`#x in o`） | `language` **+3451** | 单一最大块，且是真实代码的常见写法 |
| **M10b** | 把尺子修准（module 测试的 harness） | 让 **163+106** 条失败归位 | 在这之前 `language` 的分母是错的，选型会被误导 |
| **M10c** | 早期错误收口（应抛未抛） | `language` **~1180**、`built-ins` **~379** | 长尾，但正是本项目反复栽的那种静默错误 |
| **M11** | AOT（qjsc 等价）+ BJSON | 交付"AOT 编译"这条承诺 | 语言跑不动时，AOT 只是把一个跑不动的引擎打包 |
| **M12** | 性能与资源限制（`--memory-limit`/`--stack-size`） | NFR-3/NFR-5 | 需要先有正确的语言语义才好谈快慢 |
| **M15** | C ABI 层 | 替换现有 quickjs 使用场景 | 最后一块，边界最清楚（编号跳过 M13/M14：已取消的 Temporal/Intl） |

**"下一步做 AOT"这个答案已经被测掉了。** 需求文档把 M10 写成 AOT/BJSON，理由是"可替换
现有 quickjs 使用场景（AOT 编译）"。但只要 `import` 到的库里有 `#priv`，`qjsc` 打出来的
二进制一启动就 SyntaxError——AOT 的价值完全寄生在语言完备度上，而语言完备度现在有一个
40% 的单一缺口。先补它，AOT 顺延一位。

---

## 1. 证据：失败长什么样

`test262` 失败签名（`TEST262_SIGNATURES=1`，附带每个签名的一个例子；那个"例子"是本轮给
`test262.rs` 加的，之前只有计数，看得到 1479 也看不出该打开哪个文件）。

`test/language`，8599 条失败：

| 条数 | 形状 | 例子 |
|---|---|---|
| 1479 | `SyntaxError: private class members are not supported by the compiler yet` | `class/dstr/private-gen-meth-ary-init-iter-close.js` |
| 984 | 同上，异步用例（`$DONE` 未调用） | `class/dstr/async-private-gen-meth-ary-init-iter-close.js` |
| 807 | `private class fields are not supported by the compiler yet` | `class/elements/...-privatename-identifier-semantics-stringvalue.js` |
| 181 | 同上，异步 | — |
| 465 | `Expected a Test262Error to be thrown but no exception was thrown at all` | `assignment/dstr/array-elem-iter-nrml-close-err.js` |
| 372 | 同上，`TypeError` | `arrow-function/ArrowFunction_restricted-properties.js` |
| 303 | `expected a parse-time SyntaxError, got: …` | `arrow-function/array-destructuring-param-strict-body.js` |
| 176 | 同上，`SyntaxError` | — |
| 167 | 同上，`ReferenceError` | `arrow-function/dflt-params-ref-later.js` |
| 163 | `ReferenceError: 'assert' is not defined` | `module-code/ambiguous-export-bindings/…` |
| 106 | `could not load module filename 'module-code_FIXTURE…'` | `dynamic-import/assignment-expression/…` |
| 91 | `destructuring catch parameters are not supported by the compiler yet` | — |
| 84 + 9 | `unexpected token in expression: '*'`（`import defer`） | `import/import-defer/…` |

`test/built-ins`，2901 条失败里最大的几类：`Expected SameValue(` 245、**"应抛未抛"** 224 + 52
（TypeError/Test262Error）、`TypeError: undefined is not a function` 186、正则的
`invalid group`/lookbehind/emoji 属性 70+37+34 = 141。

三件事立刻看得出来：

1. **私有类元素是唯一的巨块。** 后面所有条目加起来才够它的一半。
2. **"应抛未抛"是第二大类**，横跨两个目录（1180 + 379）。这不是缺功能，是缺判定。
3. **163 + 106 条根本不是引擎的问题**，是我们的 test262 runner 太薄（见 M10b）。

---

## 2. M10a 私有类元素

### 2.1 现状

| 层 | 状态 |
|---|---|
| 词法 | ✅ `#x` 是 `PrivateIdentifier` |
| 解析 + 私有名环境 | ✅ `PrivateEnv` 记录声明、查重、`undefined private field '#x'`、`#x in obj` 的 AST（`ExprKind::PrivateIn`） |
| 编译器 | ❌ 三处 `SyntaxError: private class … are not supported by the compiler yet`（字段、方法、`#x in o`） |
| 运行时 | ❌ 10 个 opcode 已定义但落到 `is not implemented yet`：`get/put/define_private_field`、`check_brand`、`add_brand`、`private_in`、`scope_get/put_private_field`、`scope_get_private_field2` |

也就是说**解析器已经全部就位**，缺的是编译发射与 VM 语义——这是个有边界的活。

### 2.2 参考的机制（三件事，各有坑）

**一、私有名是一个"私有 Symbol"。** `JS_NewSymbol(ctx, desc, JS_ATOM_TYPE_PRIVATE)`——普通
symbol 值，但类型标私有。私有字段就是**实例上的自有属性**，键是这个 symbol。所以不需要新的
属性存储：`PropertyKey::Symbol` 已经能装（`object.rs:1276`）。

我们只缺一个标志：`JsSymbol` 现在只有 `description` 和 `registry_key`，加一个 `private: bool`，
并让它对 `Object.getOwnPropertySymbols`/`getOwnPropertyNames`/`Reflect.ownKeys`/`JSON`/
`Object.keys` 全部不可见。**这是 M10a 唯一会碰 `pub` 结构的地方**（加字段，0.x 下按 semver
要抬 minor ——见 §7）。

**二、私有方法/访问器不是属性，是类作用域里的变量。** 参考用
`add_private_class_field(s, fd, name, JS_VAR_PRIVATE_METHOD, is_static)` 把它加进**类作用域**
（`scope_level` 必须是类的），发射 `fclosure … set_home_object … scope_put_var_init <name>`，
读取走 `scope_get_private_field`。所以 `#m(){}` 与 `#f = 1` 是两套机制：
- 字段 → 每实例属性（`define_private_field`）
- 方法 → 类作用域里的一个函数对象（可见性由作用域链保证），加一个 brand 检查

**三、brand 放在 home object 上，靠原型链检查。** `emit_class_init_start` 在实例初始化器
开头插 `this home_object add_brand`（一段可被 patch 为 `if_false` 的空转指令，不需要时跳过）；
`check_brand` 从对象沿原型链找 `#brand` symbol。这就是为什么私有方法的"是否本类实例"判定
能穿透继承，而私有字段仍然是自有属性——两条路径分开。

我们的字段初始化器已经会 `set_home_object`，缺的是 brand 的发射与检查。

### 2.3 拆成可验证的四步

| 步 | 内容 | 验收 |
|---|---|---|
| a1 | `JsSymbol` 的 `private` 标志 + 所有"枚举自有键"的路径过滤它 | 定向测试：`Object.getOwnPropertySymbols(new C)` 不含私有键；`structuredClone`/`JSON` 同 |
| a2 | 私有**字段**：`define_private_field` / 读写 / `#x in o` | `class/elements/**` 中字段相关的 ≈1200 条 |
| a3 | 私有**方法/访问器**：类作用域变量 + `scope_*_private_field` + `add_brand`/`check_brand` | 剩余 ≈2200 条 |
| a4 | 静态私有、`this.#x` 在字段初始化器里、`#x` 在嵌套函数/`eval` 里 | 定向 + 全量差分 |

早期错误已经全对（解析器侧），所以 `#x` 相关的**负向**用例在 a2/a3 之前就已经通过——这也
意味着不能拿"总通过数涨了多少"当唯一判据，得看那 3451 条里逐条翻了多少。

### 2.4 怎么知道做对了

1. **逐用例差分**：3451 条里有 3451 个用例名，`test262-diff.py` 会给出 +N/-0 的集合比较。
2. **字节码对拍**：`disasm_diff.rs` 加一组 `private_elements_match_the_reference`。
   注意 §4 的欠账——`disasm_diff` 目前一条 class 用例都没有（构造函数与字段初始化器在
   常量池里的**先后顺序**与参考相反），所以 M10b 里要先把这件事解决，否则这一组测试写不进。
3. **行为对拍**：`crates/quickjs-cli/tests/` 加 `private_elements.rs`，整进程输出 + 退出码。

---

## 3. M10b 把尺子修准

两个 runner 缺陷让 `language` 的数字偏低，而**选型正在用这个数字**，所以先修。

### 3.1 module 测试拿不到 harness（163 条）

INTERPRETING.md §Test262-Defined Bindings：`harness/assert.js` 与 `harness/sta.js` 必须
"在测试 realm 的全局作用域"里先求值。我们给脚本测试是**拼在源文件前面**（`run_once` 的
`source.push_str(harness)`），给**模块**测试则完全不拼（`if is_module { source.push_str(&test.source) }`），
于是任何用到 `assert` 的模块测试都报 `'assert' is not defined`。

难点在于模块的顶层 `this` 是 `undefined`、且 `var`/`function` 落在模块作用域而非全局——
所以"直接拼进去"不完全等价于"先在全局作用域求值"。三条候选：

| 方案 | 代价 | 风险 |
|---|---|---|
| A. 拼进模块源文件（`harness + test`） | 最小，一处改动 | 若 harness 依赖全局 `this`/`var` 进 `globalThis` 就会失真；需先验证 `sta.js`/`assert.js` 在模块里工作 |
| B. 给 `qjs` 加 `--preload <file>`（可重复） | 新 CLI 面 | 与"CLI 选项矩阵与 `qjs.c` 一致"冲突，得在 README 里说明是测试用 |
| C. 记成 unsupported（`runner: harness in module tests`） | 最小，最诚实 | 分母变小、163 条被藏起来 |

**建议 A，验证不通过再退 C。** 仓库已有 C 的先例（`runner: $262 in module tests`），所以
C 是可接受的降落点；但 163 条能救就该救。判定用探针：随便挑 5 条 `module-code` 用例，
在参考 `qjs` 上应当通过，在本引擎上 A 方案后也应通过。

### 3.2 模块 fixture 解析（106 条）

`could not load module filename 'module-code_FIXTURE…'`：`dynamic-import` 的用例用
`import('./module-code_FIXTURE.js')` 这种**运行时拼出来的路径**，参考的 runner 有一个
"模板块名"映射（`test262/run-test262.c` 里按 `module-code_FIXTURE` 前缀重定向）。我们要么
做同样的重定向，要么记 unsupported。

### 3.3 一件**已确认不该做**的事：跳过 `import-defer` / `source-phase-imports`

这两个提案**参考实现也没有**（探针：`import defer * as ns from "x"` 与
`import source x from "x"` 在 `qjs` 2026-06-04 上都报 `from clause expected`，位置 1:14 / 1:15）。
所以它们没有 oracle，看起来该像 `Atomics` 一样跳掉。**试过了，结论是不跳**：

| 动作 | 通过数 | 说明 |
|---|---|---|
| 不跳（现状） | 15144 | — |
| 跳 `source-phase-imports` | 14901（**−243**） | 243 条全是负向用例：要求"这个语法必须被拒绝"，而整体拒绝正好满足 |
| 再跳 `import-defer` | 15051 附近（再 **−93**） | 同上，252 条里 93 条是负向 |

这正是 `MISSING_FEATURES` 里 `regexp-modifiers` 那条注释讲的事：**"整体拒绝"能满足负向
用例，跳掉特性等于把真阳性一起扔掉**。所以两个特性都不进跳过表，代价是 ≈159 + N 条正向
用例留在失败里——它们对"本引擎能跑什么"是**信息**，对"我们缺什么"也是；跳掉就什么都不是。

这条要写进 `MISSING_FEATURES` 的注释，因为下一个人一定会再想跳一次。

---

## 4. 欠账：`disasm_diff` 里没有一条 class 用例

上一轮做静态块时发现的，还没还：同一个类的构造函数与字段初始化器在父函数常量池里是**兄弟**，
参考是**惰性**创建初始化器（第一个静态字段/静态块出现时才建），我们在遍历类体之前就把构造
函数编完了，于是两者在 dump 里的**先后顺序相反**，整体文本比较必然失败。

继续比对还暴露出另外几处与静态块无关的分歧（构造函数无条件发 `get_var_ref_check
<class_fields_init>`、`push_const` vs `fclosure`、`close_loc` 选槽、`var_ref_count` 的算法）。

M10a 要字节码对拍私有元素，就必须先解决排序问题——否则那一组测试只能靠"逐函数体比较、
忽略先后"的临时手段（上一轮就是这么手工做的）。所以 M10b 顺带包含：

- 让初始化器的创建时机与参考一致（进入类体时先建，遇到第一个静态成员时落地），或
- 让 `disasm_diff` 的比较改为"按函数头配对"而不是整段文本比较，并把上面四处分歧逐一修掉。

**建议后者**：排序是表象，四处真实分歧才是里子；只改排序会让 `disasm_diff` 通过而字节码
仍然不同。

---

## 5. M10c 早期错误收口（长尾，分批）

`language` 1180 条 + `built-ins` 379 条"应抛未抛"，是同一个性质：**我们接受了一个应当被
拒绝的程序**。这类错误的可怕之处在于它是静默的——在本项目的历史上已经出现过三次
（重复声明、重复参数、`ToPropertyKey` 吞异常）。

方法照前几轮：先建差分表，再改判定。

1. **建表**：`crates/quickjs-parse/tests/early_errors.rs`，每条用例 = 源码 + 期望消息 + 期望
   位置，逐个与 `qjsc -c` 对拍。表从 1180 条失败里**按根因聚类**抽，不是逐条抄。
2. **分批修**：按"一次修一类"组织（如：方法简写的 `super` 限制、`arguments` 在
   默认参数里的限制、`let`/`const` 的 TDZ 静态判定……），每批独立一行 CHANGELOG。
3. **不追数量**：`built-ins` 里那 224 条 `TypeError` 分散在几十个内建函数上，逐个改
   `throw` 的成本远高于收益；先做 `language` 侧成族的规则。

这一项**不设完成日期**，它是持续性的：每轮收一类，直到边际收益低于别的方向。

---

## 6. 依赖顺序与并行度

```
M10b（尺子）──┐
              ├─→ M10a a1 → a2 → a3 → a4
M10b（disasm）┘

M10c       与上面全部无关，可并行（不同文件：parse 的判定 vs compile/VM）
M11 AOT    依赖 M10a 完成后才有意义
```

- **M10b 必须先于 M10a**：否则 a2/a3 的收益读数不可信（分母含 163+106 条 runner 假失败）。
- **M10a 内部严格串行**：a2 的 `define_private_field` 是 a3 方法安装的基础。
- **M10c 可并行**：它只动 `quickjs-parse` 的判定与新增测试文件。

---

## 7. 验收清单

M10a：
- [ ] `language` 3451 条私有元素用例：逐条差分 +N/−0（`test262-diff.py`）
- [ ] `disasm_diff` 新增私有元素一组，且 §4 的排序/分歧问题已解决
- [ ] `quickjs-cli/tests/private_elements.rs`：整进程输出 + 退出码 + 栈帧列
- [ ] `Object.getOwnPropertySymbols` / `Reflect.ownKeys` / `JSON.stringify` 都不泄露私有键
- [ ] `cargo test` / `check.sh` / `clippy -D warnings` 全绿

M10b：
- [ ] module 测试的 harness 就位（或明确记 unsupported 并在 README 写明）
- [ ] fixture 重定向就位（同上）
- [ ] `MISSING_FEATURES` 注释记录 `import-defer`/`source-phase-imports` 为何不跳，附实测数字

M10c：
- [ ] `early_errors.rs` 表立起来，至少一类规则全绿
- [ ] 每批修复的差分结果记进 CHANGELOG

---

## 8. 风险

| 风险 | 影响 | 对策 |
|---|---|---|
| `JsSymbol` 加字段 → 抬 minor | 版本号又一次跳 minor | 可接受：0.x 下破坏性变更就该抬 minor；但**攒在同一个版本里**，别连跳两次 |
| 私有元素牵动 GC | 私有 symbol 作为属性键是否参与环检测 | 私有键就是普通 `Gc<JsSymbol>`，随对象一起被 trace；a1 就要有 GC 测试 |
| module harness 方案 A 失真 | 数字虚高 | 先用 5 条探针在参考上确认为"应通过"，再改；不通过就退方案 C |
| M10c 无止境 | 吃掉所有时间 | 按"类"分批，每批独立可停 |
| `language` 分母再次漂移 | 前后数字不可比 | test262 提交已 pin 在 baseline 首行；每次跑都打印提交号 |

---

## 9. 待拍板

1. **AOT 顺延**：原需求文档把 M10 写成 AOT+BJSON。本文件建议先做语言收尾（M10）、AOT 变
   M11。如果"能打出二进制"比"能跑现代库"更急，就该反过来——但那样 `qjsc` 产物会在
   `#priv` 上当场失败。
2. **`--memory-limit`/`--stack-size` 的归属**：建议并入 M12（性能与资源限制）。它们现在
   解析后响亮报错，是诚实的，只是指着一个已完成的里程碑。
3. **module harness 的三条路**（§3.1）：建议 A，退 C。
