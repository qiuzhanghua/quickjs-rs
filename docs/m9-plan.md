# M9 拆解：`std`/`os` 宿主模块 + 事件循环 + CLI/REPL

状态：**待开工**（v0.6.1 之后的第一项）
验收基准：参考实现 `tests/test_std.js` 通过、`tests/test_rw_handler.js` 通过、CLI 选项矩阵与 `qjs.c` 一致

---

## 0. 结论先说

M9 不是"再写一些内建函数"，它要给引擎打开一扇它现在**没有**的门：**宿主 ↔ 引擎的双向调用**。
三处缺口按顺序挡在路上，后一个都依赖前一个：

| # | 缺口 | 现状 | 为什么挡路 |
|---|---|---|---|
| 1 | 宿主无法注册原生函数 | `Context::interp()` 是**私有**的，`NativeFn` 是裸 `fn` 指针（不能捕获状态） | 没有这一步就写不出 `std`/`os` 的任何函数 |
| 2 | 没有事件循环，只有微任务队列 | `drain_jobs()` 只排空 Promise 微任务，排空即返回 | `setTimeout` 无处挂载，"等 10ms 再跑"没有执行者 |
| 3 | `output` 是内存里的 `String` | `js_print` 往 `interp.output` 追加，CLI 跑完才 `print!` | `setTimeout` 回调里 `console.log` 必须**当场**可见，否则 `test_std.js` 的时序断言全错 |

**先把 1 做掉是对的**，因为 2 的回调（timer 到点要调 JS 函数）和 3 的改法都要用到它。
反过来先写 `std.loadFile` 之类"纯 Rust 不需回调"的函数，可以并行，但会立刻卡在 `os.setTimeout`。

---

## 1. 缺口 1：宿主 API（必须先做）

### 1.1 `NativeFn` 捕获不了状态

`crates/quickjs-pure/src/object.rs:435`：

```rust
pub type NativeFn =
    fn(&mut crate::interp::Interp<'_>, Value, &[Value]) -> crate::interp::JsResult<Value>;
```

裸 `fn` 指针。`std`/`os` 的每个函数都需要宿主状态（打开的 fd 表、timer 堆、当前工作目录、
要写往哪个 stdout），而 `fn` 指针没有地方放。

**三种改法，选 2**：

1. 全局 `static` 单例 —— 引擎就变成不可重入的（一个进程只能有一个 `Runtime`）。
   当前 `Runtime::new()` 可以多开，测试也依赖这一点。**否决。**
2. 给 `Callable` 加一个变体 `Host { f: Box<dyn FnMut(...)>, }`，把宿主状态关在闭包里。
   影响面：`Callable` 是 `enum`，每处 `match` 多一个分支；`Gc` 要能 `Trace` 一个
   `Box<dyn FnMut>`（里面的 `Gc` 得被扫描，见 1.3）。**采纳。**
3. 用 `Interp` 上一个 `host_state: Option<Box<dyn Any>>`，native 函数按下标取。
   类型擦除让每个函数都要 `downcast_ref`，错一次就 panic。**否决。**

### 1.2 公开面要开的口子

现在宿主只能 `eval`/`eval_module`/`output`/`last_value`。M9 需要长出这些：

```rust
impl<'rt> Context<'rt> {
    /// 在全局对象上装一个函数。`std`/`os` 的模块初始化就是它的批量版本。
    pub fn set_global_function(&mut self, name: &str, length: u32, f: HostFn) -> Result<()>;
    /// 调一个 JS 函数（timer 回调、`os.signal` 的 handler 都要）。
    pub fn call(&mut self, function: Value, this: Value, args: Vec<Value>) -> Result<Value>;
    /// 让宿主自己的对象能被脚本看见（`std.out` 的文件对象）。
    pub fn new_object(&mut self) -> Result<ObjectHandle>;
}
```

`HostFn` 是新的公开类型，签名与 `NativeFn` 相同但**带环境**：

```rust
pub type HostFn = Box<dyn FnMut(&mut Interp<'_>, Value, &[Value]) -> JsResult<Value>>;
```

**注意**：`Interp` 已经 `pub` 导出了（`lib.rs:64`），所以 `HostFn` 的签名不引入新的
泄漏。真正要新开的是 `Context` 上那几个方法——它们把私有的 `interp()` 收窄成三个具体用途，
而不是把 `&mut Interp` 整个交出去（那会让宿主持有 `&mut` 越过 `Context` 的借用检查）。

### 1.3 `Trace` 的坑

`Gc` 的回收靠 `Trace` 走对象图。`Callable::Host` 里闭包捕获的 `Gc` **必须被扫描**，
否则闭包持有的对象会被当成垃圾回收掉——这正是 v0.6.0 修 `WeakMap` 时踩过的同类问题。

做法：给宿主闭包要求一个显式的 `trace` 回调，或者（更简单）**约定闭包不持有 `Gc`**：
`std`/`os` 需要记住的是 fd 号、timer id、超时时间这些 **Rust 侧**的东西，JS 值一律
存进一个真正的 JS 对象（普通 `Gc<JsObject>`，本来就会被扫描）。

**选后者**，并在 `HostFn` 的文档里写死这条约束：闭包捕获 `Gc` 会造成 use-after-free。
`os.setTimeout(fn, ms)` 的回调函数存哪？存进 `Interp` 的 timer 表（表在 `Interp` 上，
`Interp` 本身是根，会被扫描）。

### 1.4 一个必须先问的问题：`console.log` 往哪写

现在 `js_print` 写 `interp.output: String`，CLI 跑完才输出。`test_std.js` 的
`test_async_promise_rejection` 要求 timer 回调里"当场"看到前面的输出。

两种做法：

- **A. 保留缓冲，但让 `output` 可被"取走"**：CLI 每跑完一轮事件循环就 `drain`
  一次 `output` 写 stdout。改动小，不碰 `js_print` 的签名；代价是宿主看到的东西仍是
  "批处理"的，一个跑很久的脚本不会有实时输出。
- **B. 让 `output` 变成 trait 对象 `Box<dyn Write>`**：`js_print` 直接写 stdout。
  与参考实现一致（它就是 `fputs` + `fflush`），但 `output()` 这个读取接口要重新设计，
  而 **测试全部靠着它**：`crates/quickjs-pure/tests/` 里有 **581 处 `eval(`**，
  其中 `builtins.rs` 一个文件就 460 处，断言形式是 `assert_eq!(eval("print(…)"), "…")`。

**倾向 A**，因为 B 要把这 581 处改成管道捕获，收益只是"实时"。但 A 必须做对：
事件循环每一轮结束后 CLI 取走增量并 `flush`，而不是等脚本结束。这样 `test_std.js`
的时序断言成立，测试也不用改。

**这个决定要在动手前定下来**，它决定 `js_print` 改不改。

---

## 2. 缺口 2：事件循环

### 2.1 现状

`Interp::drain_jobs()`（`interp.rs:1127`）排空 Promise 微任务队列，上限 100 万次防死循环，
每轮之后跑一次 `FinalizationRegistry` 回调。**它只认识微任务**，没有宏任务的任何概念。

### 2.2 参考实现的循环

`js_std_loop`（`quickjs-libc.c:4395`）结构非常简单，值得照抄：

```
loop {
    排空所有 pending job      // 微任务
    检查 unhandled rejection
    if os_poll() 返回"没有更多事件" { break }
}
```

`js_os_poll` 做三件事：取最近一个到期的 timer；没有到期的就阻塞等 fd 可读（stdin、
pipe）或等到最近 timer 到期；没有 timer 也没有 fd 就返回 -1 让循环退出。

**引擎这边要新增的东西：**

```rust
/// 宏任务：一次 `setTimeout` 到期，或一个 fd 就绪。
pub enum Task {
    Timer { id: u64, callback: Value, due_at: Instant, repeat: Option<Duration> },
    Io { fd: i32, readable: bool },
}
```

放在 `Interp` 上（和 `jobs` 并列），并给 `Interp` 加：

```rust
pub fn schedule_timer(&mut self, callback: Value, delay_ms: f64) -> u64;
pub fn cancel_timer(&mut self, id: u64) -> bool;
/// 跑一轮：排空微任务 → 取一个到期宏任务跑 → 再排空微任务。
/// 返回 false 表示再也没有任何待办（循环该退出了）。
pub fn run_one_turn(&mut self) -> JsResult<bool>;
```

`run_one_turn` 是**纯逻辑**，不做 IO。真正的"等 fd"留在 CLI 层，因为 `quickjs-pure`
是零依赖的（今天六个 crate 的外部依赖全为空，只有 workspace 内部依赖，这一点不能在
M9 悄悄破掉）。fd 轮询有三条路：

1. 引入 `libc` 调 `poll(2)` —— 一个依赖，最简单。
2. 用 `std::os::unix::io` + 手写 `syscall` —— 零依赖，但要 `unsafe`，且绑死 unix。
3. 不做 fd 轮询（见下）。

**要不要为 M9 破零依赖，是一个需要你拍板的决定。**

> 替代方案：M9 只做 timer，不做 `os.setReadHandler`/`setWriteHandler`。这样零依赖保得住，
> `test_std.js` 除了 `test_os_exec` 之外都能过，但 `test_rw_handler.js` 过不了。
> 参考实现的 `std` 也确实主要在 timer 和 fd 之间切换。

### 2.3 `os.sleepAsync` 与 `Atomics.wait`

已决定不做 `Atomics`，所以 `Atomics.waitAsync` 那条路没有。`os.sleepAsync(ms)` 可以
自己实现成"挂一个 timer 返回 Promise"，不需要 `Atomics`。这算是个好消息。

---

## 3. 缺口 3：API 面（按参考实现的清单）

参考实现注册两个模块，名字是**裸名** `std` 和 `os`（`qjs.c:118-119`），**无条件注册**——
连 `--std` 都只是额外把 `std`/`os` 挂到 `globalThis` 上（`qjs.c:506`），模块本身一直在。

### 3.1 `std`（`quickjs-libc.c:1706`）

| 分组 | 函数 | 备注 |
|---|---|---|
| 进程 | `exit`、`gc` | `exit` 要能带非 0 退出码 |
| 求值 | `evalScript`、`loadScript` | 后者是被 `test_std.js` 第一行用到的 |
| 环境变量 | `getenv`、`setenv`、`unsetenv`、`getenviron` | 注意 `unsetenv` 涉及 `unsafe` |
| 文件 | `open`、`popen`、`fdopen`、`tmpfile` | `popen` 要起子进程 |
| 格式化 | `printf`、`sprintf` | 实现 `%d %x %lx %f %*.*f %#lx` 一整套，`test_printf` 逐条比对 |
| 字符串 | `strerror` | |
| 其它 | `urlGet`、`loadFile`、`parseExtJSON` | `parseExtJSON` 解析带注释/尾逗号的 JSON |

文件对象的方法（`quickjs-libc.c:1737`）：`close`、`puts`、`printf`、`flush`、`tell`、`tello`、
`seek`、`eof`、`fileno`、`error`、`clearerr`、`read`、`write`、`getline`、`readAsString`、
`getByte`、`putByte`、`__printObject`、`__sprintObject`。

（`tell` 与 `tello` 是同一个 C 函数的两个 magic 变体，一个返回 `i64`、一个返回 `f64`；
两者都要，因为大文件的偏移超出 `i32` 精确范围。`puts`/`printf`/`sprintf` 在模块层与
文件对象上各有一套，`js_file_class` 那份的 magic 位不同。）

常量：`SEEK_SET`、`SEEK_CUR`、`SEEK_END`、`std.Error`。

### 3.2 `os`（`quickjs-libc.c:4013`）

| 分组 | 函数 | 备注 |
|---|---|---|
| 文件 | `open`、`close`、`seek`、`read`、`write`、`remove`、`rename`、`stat`、`utimes`、`realpath`、`symlink`、`readlink`、`mkdir`、`readdir`、`chdir`、`getcwd` | `stat` 返回 struct 要自己拼 |
| 终端 | `isatty`、`ttyGetWinSize`、`ttySetRaw` | `ttySetRaw` 是 REPL 行编辑的前提 |
| 定时器 | `now`、`setTimeout`、`clearTimeout`、`sleepAsync`、`sleep` | `now` 是 `performance.now()` 的来源 |
| 并发 | `signal`、`pipe`、`kill`、`dup`、`dup2`、`getpid`、`waitpid`、`exec` | **`exec` 在 `test_std.js` 里用了 5 次**，见下 |
| IO 多路 | `setReadHandler`、`setWriteHandler` | 见 2.2 的零依赖问题 |
| 常量 | `platform`、`O_*`、`SIG*`、`S_IF*` | `platform` 是字符串 |

**`os.exec` 的选项面比一个函数名所能暗示的多**（`test_std.js:241` 逐条压这些）：

```js
os.exec(["true"])                                   // 阻塞，返回退出码
os.exec(["/bin/sh", "-c", "exit 1"], { usePath: false })   // 不查 PATH
os.exec(["sh", "-c", "echo $FOO"], {
    stdout: fds[1], block: false, env: { FOO: "hello" },   // 重定向 + 环境 + 不阻塞
})                                                  // 返回 pid，靠 waitpid 收尸
os.exec(["cat"], { block: false })                  // 再 kill(pid, SIGTERM)
```

即要支持：`usePath`（默认 `true`）、`block`（默认 `true`）、`env`（替换整个环境）、
`stdin`/`stdout`/`stderr`（fd 重定向）。`block: true` 与 `block: false` 的返回值不同
（退出码 vs pid），**这不是一个 `Command::status()` 能糊过去的**。

### 3.3 全局 helper（`js_std_add_helpers`，`quickjs-libc.c:4184`）

`console`（`log`）、`performance.now`、`scriptArgs`、`print`、`__loadScript`。

`print`/`scriptArgs` **已有**（`builtins/global.rs:77` + `lib.rs:325`）。要补
`console.log` 的 flush 语义、`performance.now`、`__loadScript`。

---

## 4. CLI 与 REPL

### 4.1 现在三个 `pending()` 空洞

`crates/quickjs-cli/src/main.rs:282-296` 明确地把三件事变成"响亮失败"而不是静默接受：
`--std`、`--memory-limit`、`--stack-size`。这个设计是对的，M9 要把它们填上：

- `--std` → 注册 `std`/`os` 模块（见 3.1），并把 `globalThis.std`/`os` 挂上。
- `--memory-limit` → `Runtime::set_memory_limit` 现在**只存不用**（`lib.rs:196`）。
  要真的让 GC 在超限时报 `OutOfMemory`。这属于 `quickjs-gc` 的活。
- `--stack-size` → 同理，VM 的调用深度上限。

> `--memory-limit`/`--stack-size` 严格说是 M9 的邻域而不是核心。**建议拆出去**，
> 见第 6 节。

### 4.2 REPL

`main.rs:329` 现在是 `pending("interactive REPL", ...)`。参考实现的 `repl.js` 有 1292 行，
大半是行编辑、历史、补全。**建议不照抄**：M9 只需要一个能用的 REPL——读一行、求值、
打印、循环，`Ctrl-D` 退出，出错不退出循环。行编辑留给后面的里程碑，或用几百行的最小实现。

REPL 的难点其实是 `os.ttySetRaw` 与 `setReadHandler`（见 2.2 的零依赖权衡）。

### 4.3 选项矩阵验收

`qjs.c` 的选项面已有的部分应当保持逐字节一致。M9 要把 `--std`/`-i` 从"响亮失败"
变成"能用"，其余不动。

---

## 5. 验收清单

按依赖顺序，每一项都能独立验证：

| # | 目标 | 验证方式 |
|---|---|---|
| 1 | 宿主能注册原生函数并调回来 | Rust 单测：注册 `f`，`eval("f(1)")`，回调计数 |
| 2 | `std` 的纯 Rust 部分 | `test_std.js` 的 `test_printf`、`test_file1`、`test_file2`、`test_getline`、`test_ext_json`、`test_print_value` |
| 3 | 事件循环 + timer | `test_std.js` 的 `test_timer`、`test_async_promise_rejection` |
| 4 | `os.exec`/`waitpid` | `test_std.js` 的 `test_os_exec` |
| 5 | `os` 文件族 | `test_std.js` 的 `test_os` |
| 6 | IO 多路 | `test_rw_handler.js`（**依赖第 2.2 的零依赖决定**） |
| 7 | CLI 选项 | `--std` 可用；`-i` 进出 REPL |

`scripts/check.sh` 要新增：`test_std.js` 的逐字节 diff（和 `lang-diff.py` 一样与参考对拍）。

---

## 6. 建议的切分（四个可独立提交的块）

| 块 | 内容 | 预估 | 依赖 |
|---|---|---|---|
| **M9a** | 宿主 API：`Callable::Host`、`HostFn`、`Context::set_global_function`/`call`、`output` 的增量取走 | 中 | 无 |
| **M9b** | `std` 模块（纯 Rust 部分）+ 全局 helper | 大 | M9a |
| **M9c** | 事件循环：`Task`、`schedule_timer`、`run_one_turn`、CLI 主循环 | 中 | M9a |
| **M9d** | `os` 模块 + REPL + CLI 选项 | 大 | M9b、M9c |

**M9a 必须先做且不可并行**，其余三个之间 M9b 与 M9c 可以并行（各自只需要 M9a）。

`--memory-limit`/`--stack-size` 建议单独列成 M9e 或并入 M11，它们和"能嵌入使用"
这个可用性边界关系不大，是另一个方向（防失控）的问题。

---

## 7. 需要你拍板的三个决定

这三个都会改变实现路径，**动手前需要定**：

1. **`console.log` 的输出路径**（1.4）：保留内存缓冲、每轮事件循环取走一次（改测试少），
   还是改成 `Box<dyn Write>` 直写（与参考一致，但要改 30+ 个测试）？
2. **零依赖要不要为 fd 轮询破例**（2.2）：守住零依赖就做不了 `os.setReadHandler`/
   `setWriteHandler`，`test_rw_handler.js` 过不了；破例引入 `libc` 就只有它一个依赖。
3. **REPL 的野心**（4.2）：最小可用（几百行，无行编辑），还是照参考的 `repl.js`（1292 行）？

我的建议：**保留缓冲 + 守住零依赖 + 最小 REPL**，即 M9 交付 timer 与文件/进程 API，
把 fd 多路和行编辑明确列为"不做"（和 `Atomics` 一样写进 README 的"不做"清单，而不是
留成没说清的缺口）。
