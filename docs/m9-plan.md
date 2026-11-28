# M9 拆解：`std`/`os` 宿主模块 + 事件循环 + CLI/REPL

状态：**M9 全部完成**（M9a/M9b/M9c/M9d，提交见 git log），随 v0.7.0 发布
验收基准：参考实现 `tests/test_std.js` 通过、CLI 选项矩阵与 `qjs.c` 一致。
`tests/test_rw_handler.js` **不在验收范围内**——fd 多路已决定不做（见 2.2）。

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

**我们只抄 timer 那条腿。** `js_os_poll` 里 fd 那部分不实现（理由见本节末尾），所以
"等"这件事退化为"睡到最近一个 timer 到期"，`poll(2)` 对应的位置就是
`std::thread::sleep`——零依赖、全平台、无 `unsafe`。

**引擎这边要新增的东西：**

```rust
/// 宏任务：一次 `setTimeout`/`setInterval` 到期。
pub enum Task {
    Timer { id: u64, callback: Value, due_at: Instant, repeat: Option<Duration> },
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

`run_one_turn` 是**纯逻辑**，不做阻塞等待。真正的 `sleep` 留在 CLI 层，因为 `quickjs-pure`
是零依赖的（今天六个 crate 的外部依赖全为空，只有 workspace 内部依赖，这一点不能在
M9 悄悄破掉）。

**决定：做 fd 多路复用，依赖分层。**（本节原先写的是"不做"，根据实测证据改了。）

原结论链条是：`quickjs-libc.c` 用一次 `poll(2)` 同时等 fd 和 timer → NFR-7 禁 `libc` →
零依赖的两条替代路线（手写 syscall / 读线程）代价都过高 → 不做。链条断在第一步和第二步
之间的一个未经验证的假设上：**`poll()` 和 `AsyncFd` 的前置条件并不一样**。

- `poll(2)` 只**查询**就绪状态，**不改 fd 模式**。整个 `quickjs-libc.c` 里 `O_NONBLOCK`
  只出现两次，都是给它**自己的**唤醒 pipe 用的（`js_waker_init`，第 2372 行）；
  `js_poll_add_poll_fd` 只记录 `fd` 和 `events`，用户 fd 一个字节都不动。
- tokio 的 `AsyncFd` 要求「the file descriptor **must have the nonblocking mode set to
  true**」。要注册任何 fd 都得先 `fcntl(F_SETFL, O_NONBLOCK)`，即 `libc`——而
  `std::fs::File` 没有 `set_nonblocking`（实测 `error[E0599]`），tokio 也补不上。

也就是说 tokio 换来的东西（现成的 epoll/kqueue 封装）**不是免费的**，但也不是零收益：
手写 `poll` 要自己声明 syscall 号、绑死 unix，读线程方案要处理注销。分层之后这笔开销
落在宿主层而不脏引擎，是可接受的。

**分层（NFR-7 已据此改写）：**

| 层 | 依赖 | 职责 | 约束 |
|---|---|---|---|
| `quickjs-pure` 及其闭包 | **无** | 引擎、字节码、内建对象 | `cargo tree` 必须为空，`scripts/check.sh` 断言 |
| `quickjs-host` | `tokio`、`libc`、`windows-sys` | `std`/`os` 模块、事件循环 | 相当于参考实现的 `quickjs-libc.c` 之于 `quickjs.c` |
| `quickjs-cli` | 经 `quickjs-host` 传递 | 命令行与 REPL | 可以直接依赖宿主层 |

理由不只是"干净"：嵌入者要的是解释器，不该被迫连带一个 async runtime。这与 NFR-11
（可嵌入）是同一件事的两面。

**Windows 的边界要说清**，因为 tokio 在这里帮不上忙：

- `tokio::io::unix::AsyncFd` 是 `cfg(unix)` only。
- `tokio::net::windows::named_pipe` 只覆盖**具名**管道，`os.pipe()` 是匿名管道。
- 参考实现自己的 Windows 分支（`js_os_poll`，第 2485 行）用 `WaitForMultipleObjects`，
  但放进等待列表的**只有 `fd == 0`（stdin）**：

  ```c
  if (rh->fd == 0 && !JS_IsNull(rh->rw_func[0])) {
      handles[count++] = (HANDLE)_get_osfhandle(rh->fd);   /* stdin */
  }
  ```

  并且 `Makefile` 第 474 行把这两个测试从 Windows 上整个排除：

  ```make
  ifndef CONFIG_WIN32
      $(WINE) ./qjs$(EXE) tests/test_std.js
      $(WINE) ./qjs$(EXE) tests/test_rw_handler.js
  endif
  ```

所以目标是与上游**等价**而不是超越：Unix 上任意 fd 可注册（`AsyncFd`），Windows 上只有
stdin（`windows-sys` 的 `WaitForMultipleObjects`），`test_rw_handler.js` 在 Windows 上
与上游一致地跳过。这不是妥协，上游本身就没有那个能力。

> 参考实现里 `std` 确实主要在 timer 和 fd 之间切换，砍掉 fd 之后事件循环只剩 timer
> 一条腿——但这一条腿正是 `setTimeout`/`sleepAsync`/`performance.now` 需要的，M9 的
> 目标（让“等一会儿再跑”有执行者）不受影响。

**外部佐证：rquickjs 也不做这事。** 调研过 `DelSkayn/rquickjs`（现成最大的 Rust↔QuickJS
绑定），它不是重写引擎，是把 C 版 `quickjs.c` vendor 进来包一层 FFI。它的 `build.rs:143`
只编译 4 个文件：

```rust
let source_files = ["libregexp.c", "libunicode.c", "quickjs.c", "dtoa.c"];
```

`quickjs-libc.c` **在包里躺着但没编**，预生成的绑定里也搜不到任何 `js_os_*` / `js_std_loop`
符号。也就是说它同样没有 `std`/`os`、没有事件循环、没有 `poll(2)`：它的 `execute_pending_job()`
是**需要宿主手动调**的裸函数，`setTimeout` 只写在 `#[cfg(test)]` 里、靠 tokio 的
`tokio::time::sleep` 实现。**结论：那个生态里没有"零依赖 fd 多路"的现成范例可以照抄**，
它走的是"把定时器和事件循环外包给 tokio"这条路，而 tokio 对我们的 NFR-7 是一票否决。

（它唯一值得借鉴的是 M9a：`Func::new` 把闭包 `Box` 起来作为函数对象自带的 opaque 数据，
生命周期交给 JS 对象的 finalizer。但那依赖 `JS_NewClassID` 给 `RustFunction` 分配 class ID
和 finalizer，而我们的 `crates/quickjs-pure/src/` 里还没有任何自定义 class 机制——
详见 1.1。）

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
| IO 多路 | `setReadHandler`、`setWriteHandler` | **做**，见 2.2：Unix 用 `AsyncFd`，Windows 用 `WaitForMultipleObjects`（仅 stdin，与上游一致） |
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

### 4.2 REPL —— 照参考的 `repl.js`（1292 行）

`main.rs` 现在是 `pending("interactive REPL", ...)`。**决定照参考实现**，行编辑、历史、
补全都要。

原判断"`ttySetRaw` 与 `setReadHandler` 需要 fd 多路 + termios，所以只能面向行"**不成立**。
逐条核实过 `repl.js` 的实际用法：

| 依赖 | `repl.js` 怎么用 | 原判断 | 实际 |
|---|---|---|---|
| `setReadHandler`（L137、L861） | 唯一用途是"stdin 有字节时读一次"，回调体就是 `os.read(fd, buf, 0, len)` 再逐个 `handle_byte` | 必须 fd 多路 | **可替代**：raw 模式下阻塞读 1 字节，循环喂 `handle_byte`，语义等价 |
| `ttySetRaw`（L128） | 关回显、关行缓冲 | 必须 termios / `libc` | **可替代**：`Command::new("stty").args(["raw", "-echo"])` + `Stdio::inherit()`，零 `unsafe`、零 `libc` |
| `os.signal(SIGINT)`（L133） | 装上后调 `handle_byte(3)` | 需要信号系统 | **不需要**：raw 模式下 Ctrl-C 就是字节 3，`repl.js` 自己就是按字节处理的 |

即"不用 fd 多路、不用 `libc`、不用 `unsafe`"也能拿全行编辑/历史/补全。代价只有 Windows：
`stty` 不存在，降级为行缓冲（NFR-8 仍满足，功能退化而非不可用）。

**2.2 的"fd 多路不做"结论不变**，但它挡住的只是 `os.setReadHandler`/`setWriteHandler` 这两个
API 和 `test_rw_handler.js`，**不挡住 REPL**——REPL 不需要在一堆 fd 里等，它只需要读 stdin。

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
| 6 | IO 多路 | `test_rw_handler.js`（Unix 上；Windows 上按上游的 Makefile 跳过） |
| 6 | CLI 选项 | `--std` 可用；`-i` 进出 REPL（面向行，无行编辑） |

`scripts/check.sh` 要新增：`test_std.js` 的逐字节 diff（和 `lang-diff.py` 一样与参考对拍）。

---

## 6. 建议的切分（四个可独立提交的块）

| 块 | 内容 | 预估 | 依赖 |
|---|---|---|---|
| ~~**M9a**~~ | ~~宿主 API~~ — **已完成**，见第 8 节 | 中 | 无 |
| **M9b** | `std` 模块（纯 Rust 部分）+ 全局 helper | 大 | M9a |
| **M9c** | 事件循环：`Task`、`schedule_timer`、`run_one_turn`、CLI 主循环 | 中 | M9a |
| **M9d** | `os` 模块 + REPL + CLI 选项 | 大 | M9b、M9c |

**M9a 必须先做且不可并行**，其余三个之间 M9b 与 M9c 可以并行（各自只需要 M9a）。

`--memory-limit`/`--stack-size` 建议单独列成 M9e 或并入性能那一步，它们和"能嵌入使用"
这个可用性边界关系不大，是另一个方向（防失控）的问题。

---

## 7. 需要拍板的两个决定 — 已定

1. **`console.log` 的输出路径**（1.4）：**定了 B，直写**。`Interp` 加
   `output_sink: Option<Box<dyn Write>>`，`print` 每行写进去并 `flush`，与参考的
   `fputs` + `fflush` 一致。同时**保留** `output: String` 累积全量——它是 `Context::output()`
   的来源，**595 处测试断言因此一处没改**。缓冲无界增长由 `Context::take_output()` 解决，
   CLI 每次跑完取走（已写入 stdout，取走只是释放内存）。
2. **REPL 的野心**（4.2）：**定了照参考的 `repl.js`**。见 4.2 的更正——原判断"必须 fd 多路 +
   termios"不成立。

### 已定，不再讨论

- **fd 多路复用：做**（2.2，已推翻原结论）。依赖分层：引擎零依赖不变，`tokio`/`libc`/
  `windows-sys` 只进 `quickjs-host`。
- **`Atomics`/`Temporal`：不做**（已在 README 的"不做"清单里）。

我的建议：**保留缓冲 + 最小 REPL**，即 M9 交付 timer 与文件/进程 API，把 fd 多路和行编辑
明确列为“不做”（和 `Atomics` 一样写进 README 的“不做”清单，而不是留成没说清的缺口）。

---

## 8. M9a 已完成（宿主 API）

### 8.1 交付

| 项 | 位置 |
|---|---|
| `HostFn`（带环境的原生函数） | `object.rs`，文档写死"闭包不得捕获 `Gc`"及其原因 |
| `Callable::Host { func, own_instance }` | `object.rs`，`Trace` 分支为空并注明依据 |
| `Interp::new_host_function` / `new_host_ctor` | `interp.rs` |
| `Context::set_global_function` / `set_global_constructor` | `lib.rs` |
| `Context::set_global` / `new_object` / `set_property` / `new_string` / `new_number` / `new_boolean` | `lib.rs` |
| `Context::call` | `lib.rs` |
| `Context::set_output_sink` / `take_output` | `lib.rs` + `builtins/global.rs` |
| 验收测试 8 条 | `crates/quickjs-pure/tests/host.rs` |

### 8.2 三个实现细节与原因

1. **闭包要放在 `Rc<RefCell<…>>` 里**，不是 plan 写的裸 `Box`。调用时要 `&mut Interp`，
   而调用点此刻正持有该函数对象 `data` 的 `RefCell` 借用；闭包本身可能再次碰到同一个对象。
   把 `Rc` 克隆出去、先释放借用再调用，才不会 panic。

2. **`Trace` 什么都不做是安全的，但只在一条约定下成立**：闭包捕获 `Gc` 会被回收器看不见，
   导致 use-after-free。这条约定写进了 `HostFn` 的文档，并有测试
   （`a_host_function_and_its_state_survive_collection`）显式 `run_gc()` 后仍能用。
   注意那条测试断言了 `collected > 0`——否则它什么都没证明。

3. **`set_script_args` 原本会被静默丢弃**。`Context::interp()` 在首次使用时把
   `script_args`/`module_loader` take 走；`set_output_sink` 提前创建了 interpreter，
   于是之后设置的 `scriptArgs` 无处可去，`print(JSON.stringify(scriptArgs))` 直接
   `ReferenceError`。现在 `set_script_args` 对已存在的 interpreter 也会安装。
   这是 `Context` 的一个通用陷阱（任何"先装别的再设参数"的宿主都会踩），已在文档注明。

### 8.3 与 plan 的一处偏差

plan 1.4 里 B 的代价写的是"要改 30+ 个测试、581 处断言"。实际不必：
保留 `output: String` 作为 `output()` 的来源，**另加** sink 直写。
两者并存，实时性和测试 API 都拿到，代价是 CLI 需要 `take_output()` 防无界增长。

---

## 9. M9c 已完成（事件循环 + timer）

### 9.1 切分

| 层 | 内容 |
|---|---|
| `quickjs-pure` | `Task::Timer`、`schedule_timer`、`cancel_timer`、`next_timeout`、`run_one_turn`、`Take_timer_errors` — 纯逻辑，不阻塞 |
| `quickjs-host::eventloop` | `run_until_idle`、`run_once`，唯一的 `thread::sleep` 在这里 |
| `quickjs-host::timers` | `os.setTimeout`/`os.clearTimeout` 的注册与参数marshal |

`run_one_turn` 返回 `Turn::{Ran, Wait(Duration), Done}` 而不是 `bool`。原计划写的是
「返回 false 表示循环该退出」，但一个裸 `bool` 会诱使调用方不睡就死循环空转，把 CPU
烧到 timer 到期。三个具名分支让「等多久」成为类型的一部分。

### 9.2 对拍发现四处偏差（都已改）

计划里没写这些，是拿探针跑 `qjs --std` 才看到的：

| 用例 | 参考实现 | 我最初的实现 |
|---|---|---|
| `typeof os.setInterval` | `"undefined"`——根本没有 | 我加了 `setInterval`，还带 repeat 语义 |
| `os.setTimeout(42, 1)` | 立刻抛 `TypeError: not a function` | 排到触发时才报 |
| `os.setTimeout(f, 0, "x", "y")` | 回调**收不到**参数（`undefined,undefined`） | 我做了透传 |
| `os.setTimeout(f,1)` 的返回值 | `JS_NewInt32`，`JSON.stringify` 得 `1` | 返回 float |

`setInterval` 这一条最值得记：浏览器心智会让人反射性地加上它，但参考实现里
`grep -c setInterval quickjs-libc.c` 是 0。加了它等于给引擎一个参考实现产不出来的
timer，差分测试根本没有东西可比。`Task` 里的 `repeat` 字段因此删掉。

### 9.3 端到端

`qjs --std` 跑同一个探针脚本，输出与参考**逐字节一致**（含顺序、微任务在 timer 之间
排空、参数为 `undefined`）。`cargo test` 全绿，`check.sh` 全绿。

### 9.4 还剩

- `os.setTimeout` 的 id 复用与 `INT32_MAX` 回绕已实现，但没有测到回绕本身（需要
  INT32_MAX 次调度，不值得为它跑一次测试）。
- `sleepAsync`/`performance.now` 属 M9d。

---

## 10. M9b 进展（std/os 的纯 Rust 部分）

### 10.1 已完成

| 项 | 位置 | 验收 |
|---|---|---|
| `printf`/`sprintf` 全套格式 | `quickjs-host/src/printf.rs` | 89 条用例与 `qjs --std` **逐字节一致** |
| `std.parseExtJSON` | `json.rs` 加 `ext` 标志 | 参考的 `test_ext_json` 原样通过 |
| `os` 的文件/目录/进程面 | `quickjs-host/src/os.rs` | 31 条用例逐字节一致 |
| `os.setTimeout`/`clearTimeout` | `quickjs-host/src/timers.rs` | 见第 9 节 |

### 10.2 `parseExtJSON` 为什么不用 serde

实测 `serde_json` 对扩展方言需要的 **12 项特性全部拒绝**：注释、行注释、裸键、
尾逗号、单引号、`+1`、`0o10`、`0xa0`、`Infinity`、`NaN`、`.1`、换行续行。
serde 实现的是 JSON 的**严格子集**，而这里需要**超集**。

参考实现也不是「预处理成标准 JSON 再解析」，而是
`JS_ParseJSON2(ctx, str, len, "<input>", JS_PARSE_JSON_EXT)` —— **同一个解析器加标志位**。
所以照做了：`Parser` 加 `ext: bool`，每处放宽各自门控。这样 `JSON.parse` 保持严格，
两条路径共享同一套错误位置。

顺带修掉一个**预存在 bug**：`number()` 从不校验首字符，而 `f64::from_str` 接受 `+1` 和
`.1`，所以 `JSON.parse("+1")` 和 `JSON.parse(".1")` 一直是成功的。参考实现两者都拒绝。

### 10.3 `os` 的失败约定有**两套**，不是一套

这是本轮最大的发现。第一次实现用了一致的一套，**31 条用例里错了 13 条**。

| 约定 | 函数 | 失败形态 |
|---|---|---|
| 二元组 | `stat lstat readlink realpath readdir getcwd utimes` | `[value, **正** errno]`，`make_obj_error` |
| 裸值 | `open close seek read write mkdir remove rename chdir` | **负** errno，`js_get_errno` = `if (ret==-1) ret=-errno` |

```text
> os.stat("/nope")        [null, 2]      正 errno，在二元组里
> os.mkdir("/tmp", 0o755) -17            负 errno，裸值
> os.open("/tmp", O_RDONLY) 3            就是 fd 本身
```

另有两处不规则：`os.isatty` 返回裸 Boolean，`os.pipe` 返回裸 `[r, w]`（失败是 `null`）。
这些都不是笔误，是参考实现本来的样子；期望值全部来自 `qjs --std` 而非注释。

还有一处：`os.remove` 是 `remove(3)` 而不是 `unlink(2)`，所以**能删目录**
（`unlink` 会报 EPERM，`remove` 会退到 `rmdir`）。

### 10.4 rustix，以及为什么这里没有 `unsafe`

`os` 的 syscall 面用 `rustix`（3 个 crate：`bitflags`/`errno`/`libc`，加平台表
`linux-raw-sys`）。理由是 NFR-6 把 `unsafe` 限制在四个引擎模块，宿主层直接调 `libc`
会在 20 来个函数上逐处破例。

还有一处本来也需要 `unsafe`：把脚本给的 fd 数字变回描述符
（`BorrowedFd::borrow_raw` 是 `unsafe`）。解法是 `fdtable`：宿主持有 `OwnedFd`，
只把数字交给脚本，读写时用 `OwnedFd::as_fd()`（安全）。0/1/2 走
`std::io::stdin()` 的 `AsFd`。**结果：`os.rs` 里一个 `unsafe` 都没有**，NFR-6 不需要改。

副作用是「宿主不持有的 fd 报 EBADF」。参考实现会真的去 `read(999)` 拿到 EBADF，结论相同。

### 10.5 还没做

- `std`：文件对象（`open`/`tmpfile`/`popen`/`fdopen` + 19 个方法）、环境变量、
  `loadFile`/`loadScript`/`evalScript`、`strerror`、`urlGet`、`exit`、`gc`、
  `__sprintObject`/`__printObject`
- `os`：`exec`/`waitpid`/`kill`/`dup`/`dup2`/`signal`/`now`/`sleep`/`sleepAsync`、
  `ttyGetWinSize`/`ttySetRaw`、`setReadHandler`/`setWriteHandler`

**`std.setenv`/`unsetenv` 有个平台变化要注意**：edition 2024 里
`std::env::set_var`/`remove_var` 都是 `unsafe`（真实的 `setenv` 改进程全局状态）。
`rustix` 不管环境变量，没有 crate 能绕开。需要加 `unsafe` + `SAFETY` 注释，
并相应放宽 NFR-6，或者把这两个函数列入「不做」。

### 10.6 `std` 文件对象（17 个方法）

| 项 | 位置 | 验收 |
|---|---|---|
| `std.open`/`popen`/`tmpfile`/`fdopen` | `quickjs-host/src/std.rs` | 模式校验 + `.errno` 报告，与参考一致 |
| 17 个方法 | `quickjs-host/src/stdio.rs` | 18 条对拍全对 |
| `std.out`/`in`/`err`、`SEEK_*` | 同上 | 形状与参考相同 |

**参考自己的 `test_file1`/`test_file2`/`test_getline`/`test_popen` 原样通过。**

三件值得记下的事：

**1. 为什么不能用 `BufReader` 包一层。** `FILE` 是带位置的缓冲流，`test_file1` 的写法是
「写 → seek 到末尾 → `tell()` 拿大小 → seek 回开头 → 读出同样的字节」。`tell()` 必须同时
反映「已写」和「已读回」的字节，那是流的逻辑位置，不是描述符的偏移——缓冲写在描述符那边
还没发生。所以 `stdio.rs` 自己维护读缓冲、写缓冲和位置，`seek` 靠丢弃缓冲实现。

**2. `fdopen` 逼出一次架构修正。** 参考的 `test_os_exec` 里是
`std.fdopen(os.pipe()[0], "r")`——`std` 必须看见 `os` 造出的描述符。原先两个模块各有一张
`FdTable`，现在改成**一张共享表**（`FdTable::shared()`，`os::install` 与 `std::install`
都接它）。`fdopen` 从表里 `take` 走所有权，正好对应 `fdopen(3)` 的语义：描述符归 `FILE`
所有，`fclose` 关闭它。

**3. `std.out` 走 sink 而不是 fd 1。** 宿主已经拥有 stdout（`print`/`std.printf` 都写
sink），若 `std.out` 直接写 fd 1 就是同一个流上的两个缓冲，`std.out.puts("A"); print("B")`
可能输出 `BA`。写 sink 才保得住顺序——对拍里 `AB / xy1` 两行确认了这点。

**一处刻意的偏差**：实例要在大小的槽表里指认自己，原型方法只能靠实例上的一个属性做到，
所以 `Object.getOwnPropertyNames(f)` 会多出 `"\u{0}std.file"`（参考是空）。换成 symbol 键
只是把偏差从 `getOwnPropertyNames` 挪到 `getOwnPropertySymbols`，并不消失。`Object.keys`、
`JSON.stringify`、`hasOwnProperty`、`for...in` 全部与参考一致。

**另外补上的 `std` 成员**：`loadFile`、`loadScript`、`evalScript`、`strerror`、`getenv`、
`getenviron`、`exit`、`gc`——都不需要 `unsafe`。`strerror` 的文字取自
`std::io::Error`（它就是 `strerror_r`），去掉 `" (os error N)"` 后缀，所以
`std.strerror(0)` 仍是 macOS 的 `"Undefined error: 0"` 而不是某张可移植表。

**`setenv`/`unsetenv` 仍未做**，因为 edition 2024 里 `std::env::set_var` 是 `unsafe`。
`loadScript` 当前走 `eval_in_global`，栈里显示 `<input>` 而不是文件名，与参考有差；修它需要
一个带名字的 eval 入口。

### 10.7 `__sprintObject` / `__printObject`

`quickjs-host/src/print.rs`，`JS_PrintValue` 的移植。**105 条用例逐字节一致**，含参考自己的
`test_print_value`。

它不是 `JSON.stringify`，也不是 `console.log`：它遍历 quickjs 的**对象形状**，所以规则都是
从 `js_print_value`/`js_print_object` 抄的，不是发明的：

```text
[ 1, 2, 3 ]              数组方括号内侧有空格
[  ]                     空数组也是两个空格
{  }                     空对象两个空格
{ a: 1, "a b": 2 }       标识符键不加引号，否则加
{ a: { b: [Array] } }    超过 max_depth(2) 打类名
{ self: [circular 0] }   环标出首次出现的层级
[ 0: 1, 2: 3 ]           稀疏数组打 下标: 值
[ 1, 2, 3, x: 4 ]        数组上的非索引属性也会打
Map(1) { 1 => 2 } / Set(2) { 1, 2 } / Uint8Array(3) [ 1, 2, 255 ]
```

默认值也是参考的：`max_depth=2`、`max_string_length=1000`、`max_item_count=100`。
超限会写进输出（`... 3 more items`、`... 5 more characters`），所以这三个数字必须精确。

几个容易错的点：

- `-0` 打 `-0`（`JS_DTOA_MINUS_ZERO`），而 `String(-0)` 是 `0`。其余浮点走 JS 的
  Number→string，所以 `1e+21`/`1e-7`/`Infinity` 都对。
- **键序是定义顺序，不是规范顺序**。`{[Symbol("t")]: 1, y: 2}` 参考打 symbol 在前，
  而 `Reflect.ownKeys` 的规范顺序会把 symbol 放到最后。为此给引擎加了
  `JsObject::own_property_keys_in_definition_order()`，`own_property_keys` 保持规范顺序不动。
- `BigInt64Array` 的元素打裸整数（参考用 `%lld`，不走 BigInt 路径），所以没有 `n` 后缀。
- `new Date(NaN)` 打 `Date {  }`——`toISOString` 抛异常，参考退回默认对象形态。
- 包装对象（`new String("s")`）打 `String {  }`，体为空。

**一处刻意的取舍**：`Error` 的栈来自 `stack` 属性，因此我们的栈里函数名/行号与参考不同
（两个引擎的位置信息本来就不同）。参考自己的 `test_print_value` 里那条断言也只能靠
`remove_line_nums` 才成立，所以这里不比栈内容。

### 10.8 `setenv`/`unsetenv`：NFR-6 的唯一宿主例外

15 条对拍全对。走的是「加 `unsafe` + `SAFETY`」而不是「不做」，理由是 `os.exec` 的 `env`
选项下一步同样绕不过去。

**为什么没有安全替代**（三条都实测过，不是推测）：

| 方案 | 结论 |
|---|---|
| `rustix` | 故意不包环境变量，grep 其源码零命中 `setenv`/`unsetenv` |
| `dotenv`/`dotenvy` | `dotenvy 0.15.7` 的 `src/iter.rs:35,52` 内部就是 `env::set_var`——是它**之上**的一层，且额外承担 `.env` 文件解析，而 `std.setenv` 拿到的是分开的两个字符串，没有文件可解析 |
| 宿主侧 overlay map | 安全，但子进程看不到。实测参考 `std.setenv("FOO","bar")` 之后 `std.popen("sh -c 'echo $FOO'")` 输出 `bar`；overlay 得合并进 `popen`/`os.exec`/`os.exec` 的 `env` 三处，四处不变量替代一次 `unsafe` |

**为什么此处健全**：解释器全程单线程；`quickjs-host` 声明了 `tokio` 但不启动 runtime、不起
工作线程。`unsafe` 收敛在 `quickjs-host/src/env.rs` 一个模块两个调用，为的就是这个论证有
唯一审阅点。

**顺带挡掉一个真 bug**：`set_var`/`remove_var` 对空名、含 `=`、含 NUL 的名字是 **panic**
而非返回错误，脚本能造出这三种。`setenv(3)` 对这些返回 `EINVAL` 且参考忽略返回值，所以
`env.rs` 在调用前拒绝，语义仍是「静默无效」。

### 10.9 `scripts/check-unsafe.sh`：把 NFR-6 从句子变成门禁

NFR-6 原来只是一句散文，于是它和代码早就对不上了。新脚本逐文件扫描 `crates/*`，`unsafe`
只许出现在白名单里。第一次跑就抓出两处：

- `quickjs-pure/src/builtins/math.rs` 的 `static mut STATE`（`Math.random` 的 xorshift）
  ——**改成 `AtomicU64`**，零成本消掉 `unsafe`，`Relaxed` 在单线程下不花钱。
- `quickjs-pure/src/value.rs` 的 `from_utf8_unchecked`——**加入白名单**。它是字符串表示层，
  `valid` 只由编码器设置，每次访问改成 `from_utf8` 会在每个属性读取的热路径上加一次扫描。

所以 NFR-6 现在写的是五个引擎模块（加了 `value`）加一处宿主例外，且这句话可执行。

---

## 11. M9d: `os` 的进程与时间面

### 11.1 已完成

| 项 | 验收 |
|---|---|
| `os.exec`（`block`/`usePath`/`file`/`cwd`/`env`/`stdin`/`stdout`/`stderr`） | 与参考逐字节一致，含 `test_os_exec` 的管道重定向模式 |
| `os.waitpid` / `os.kill` / `os.dup` / `os.dup2` | 同上 |
| `os.now` / `os.sleep` / `os.sleepAsync` | 同上 |
| `os.ttyGetWinSize` / `os.ttySetRaw` | 同上 |

### 11.2 `os.exec` 的几个语义

- 退出码是**退出状态**：正常退出是 `code`，被信号杀死是 **`-信号号`**，程序起不来是
  `127`（参考的子进程在执行失败后 `_exit(127)`）。
- `block: false` 返回 pid。子进程要活到后面的 `waitpid` 才能被收集，所以 `Child` 句柄
  是**故意泄漏**的（`mem::forget`）——`waitpid` 就是脚本收集它的方式。
- `env` **替换**整个环境而不是追加：实测参考里 `env: {OTHER:"x"}` 之后子进程看不到
  `PATH`。
- `usePath: false` 对应 `execve` 不搜 `PATH`。`Command` 总是搜，所以用清空 `PATH` 的
  方式复现，而不是写第二套 exec。

### 11.3 两处刻意偏差（都写了理由并钉了测试）

**1. `os.kill(pid, 0)` 报 `EINVAL`。** `rustix::process::Signal` 是 `NonZeroI32` newtype，
**结构上表达不了** `kill(pid, 0)` 需要的那个 `0`。rustix 自己指向 `rustix-libc-wrappers`，
而该 crate 在本项目用的 registry 上**不存在**（实测 `cargo fetch` 失败）。`from_raw_unchecked`
存在，但其文档明说「结果不得用于发送信号」。为一次探针加依赖或加 `unsafe` 都不划算。

**2. `os.waitpid(pid, WNOHANG)` 未就绪时我们返回状态 `0`。** 参考返回的是**未初始化的栈垃圾**
——`js_os_waitpid` 只在 `ret < 0` 时写 `status = 0`，`ret == 0` 那条路径留着未初始化变量。
实测三次运行分别得到 `55289120` / `60908832` / `21718304`。那是参考的 UB，不复刻；我们的
`0` 是稳定且正确的值。

### 11.4 还没做

- `os.signal`：参考装一个 C 信号处理器再回调 JS。信号处理器里调 JS 的异步信号安全性是
  真问题，需要单独设计（参考自己也有 `is_main_thread` 的守卫）。
- `os.setReadHandler`/`os.setWriteHandler`：需要事件循环把 fd 就绪纳入 `Task`/`Turn`。
  `fds.rs` 已有文档说明 Linux/macOS/Windows 三平台各自的边界。
- REPL：CLI 选项已齐全，`-i` 目前报 `not implemented yet`。

---

## 12. M9d: fd 就绪、信号，以及一个挡路的编译器 bug

### 12.1 已完成

| 项 | 验收 |
|---|---|
| `os.setReadHandler` / `os.setWriteHandler` | 参考自己的 `test_rw_handler.js`（100 轮读写握手）通过 |
| `os.signal` | 三个形态（函数 / `null` / `undefined`）与参考一致 |
| `Task`/`Turn` 接纳 fd 就绪 | 新增 `Turn::Poll`，编译器强制每个宿主分支处理它 |

**`Turn::Poll` 单独成一个变体，不复用 `Turn::Wait`**：宿主若把「有 fd 注册」当成「睡一觉」，
处理器会永远不触发——那正是本项目视为最坏的失败形态。用独立变体让两者不可能被混淆。

引擎零依赖不能 `poll(2)`，所以分工是：引擎报「哪些 fd、哪个方向」（`Interp::pending_fds`），
宿主 `poll(2)`（`fds::poll`，用 `rustix::event::poll`），就绪后回调
（`Interp::fire_fd`）。参考每轮只调一个处理器然后重启循环（`goto done`），因为处理器可能改
注册表；这里照做。

**顺带修掉一个真 bug**：`std.in.fileno()` 拿到的描述符在 stdio 槽表里，而 `os.read` 只认
fd 表——`repl.js` 正是 `os.read(std.in.fileno(), …)`，会一直读到 `-EBADF`。现在 fd 表挂了
一个槽表视图（`FdTable::attach_slab`），两个方向都通了。

### 12.2 `os.signal`：第二处 `unsafe` 例外

信号处理器是操作系统回调 Rust 函数，没有安全包装可言。处理器只在一个
`AtomicU64` 上置位，真正的 JS 调用由事件循环做——**这正是参考的做法**
（`os_signal_handler` 只置 `sh->ishandling`，`os_poll` 发现）。

机制选择有个实测发现：**`rustix` 1.1.6 把 `runtime` 模块标成 `pub(crate)`**，
所以 `kernel_sigaction` 对外不可达。改用 `libc::signal`——它本来就是宿主层的声明依赖
（当初为 tokio/`AsyncFd` 计划带的，一直没用上），而且参考调的就是 `signal(3)`。

### 12.3 挡路的编译器 bug（预存在，本次未修）

`repl.js` 跑不起来，卡在 `plen = prompt.length` 报 `cannot read property 'length' of undefined`。
追下去是一个**静默返回错误值**的代码生成 bug，与 M9d 无关：

```js
(function(){ var p = ""; function f(){ p = "x"; if (false) { p = "a"; } return p; } f(); })()
// 参考："x"    我们：undefined
```

最小复现的三个必要条件：**闭包变量** + 中间有标签（`if`/块）+ `return <该变量>`。
局部变量不复现（`var q` 版本正确返回）。

字节码直接暴露了它——`return p` 根本没被编译成读取：

```text
function: f
  closure vars: 0: var p [loc0]
  opcodes:
        push_atom_value x
        put_var_ref0 0: p      ← 消耗了值
        return                 ← 栈上是空的
```

对照正确版本（`p="x"` 之后直接 `return p`，无 `if`）：

```text
        push_atom_value x
        set_var_ref0 0: p      ← 保留值（put→set 折叠加返回）
        return
```

`emit_var` 里有一条窥孔优化：`put_x v` 紧跟 `get_x v` 折叠成 `set_x v`（存值并留在栈上），
且**标签会取消这次折叠**（`label_since_insn`）。但取消折叠之后，后续仍然没有发出
`get_var_ref`——两处记账不一致：存值用了消耗型指令，而 `return` 仍以为值在栈上。

危险之处在于它**不报错**，只是给出 `undefined`，所以大量测试会碰巧通过（值恰好等于前一条
语句留在栈上的东西）。这也解释了为什么 `if (true)` 版本"看起来对"——它返回的是上一条赋值
留下的值，不是读出来的。

修复属于编译器子系统的独立工作（需要 test262 的 `language/` 用例做回归，而当前 checkout 是
sparse 的 `built-ins`），不适合在这轮末尾仓促改。**REPL 因此暂缓**：它依赖参考的 `repl.js`
原样运行，而这个 bug 就在它的启动路径上。

### 12.4 还没做

- REPL：等上面的 bug 修掉，再把参考的 `repl.js` 原样跑起来（只差把两行 `import` 换成全局绑定，
  其余依赖 `os.setReadHandler`/`signal`/`ttySetRaw`/`std.in` 都已就位）。

### 12.5 顺带修掉的一个测试竞态

`scripts/check.sh` 开始必现地挂在 `verify-claims.py`（`too many values to unpack`）。
原因是 `disasm_diff.rs` 的多个 `#[test]` 并行执行，各自以 append 方式打开同一个
`disasm-cases.txt` 写一行——`O_APPEND` 只保证偏移更新是原子的，不保证写本身不交错，于是
一行被拼成了多列。加一把锁、一次 `write_all` 即修。单独跑 `verify-claims.py` 从不复现，
因为只有 `check.sh` 里的并行度才够。

---

## 13. REPL（参考的 `repl.js` 原样运行）

M9d 结束时 REPL 被一个编译器 bug 挡住。修掉它之后，`qjs -i` 现在跑的是**参考自己的
`repl.js`**——行编辑器、语法着色、补全、`\h` 帮助全是参考的代码，不是重写的。

### 13.1 编译器 bug（先修的那个）

见 §12.3。修法在 `fold_dup_patterns`：被跳转指向的 `drop`/`get` 不能折叠掉。

`emit_var` 的窥孔把 `dup; put_x; drop; get_x` 折成 `set_x`（存值并留在栈上），只在
**顺序落下**时等价。当跳转**落在**那条 `get_x` 上（`if` 体结束、`switch` case、循环头），
删掉它就把跳转重定向到后一条指令，于是 `return` 在空栈上执行——存值那一步已经把值消耗掉了。

危险在于它不报错，只给 `undefined`，所以大量测试碰巧通过（值恰好等于前一条语句留在栈上的
东西）。这也解释了为什么 `if (true)` 版本"看起来对"。

回归覆盖两层：
- `crates/quickjs-pure/tests/run.rs` 的 `a_jump_target_after_a_store_is_not_folded_away`
  （运行时行为，`if`/`switch`/`while` 三种标签位置）
- `disasm_diff.rs` 的 `a_jump_target_after_a_store_stays_put`（6 条字节码逐字节对拍）

test262：`test/language` +4 修复（`statements/switch/S12.11_A1_T*`），
`test/built-ins` 零变化，两边都 **-0 回归**。

### 13.2 为跑通 REPL 补的四处

**`std.in.fileno()` 必须是 0。** 原先 `std.in` 持有 `dup(0)`，`fileno()` 报 3。`repl.js` 把
读处理器装在 `os.setReadHandler(std.in.fileno(), …)` 上，于是等的是一个终端永远不写的描述符
——打印提示符后永久静默。改成 `Kind::Stdin`：借 `rustix::stdio::stdin()` 的
`BorrowedFd<'static>`，报告 0，且仍然不拥有它（脚本关 `std.in` 不影响进程 stdin）。

**`std.evalScript` 的 `async` 选项。** 参考的 `JS_EVAL_FLAG_ASYNC` 把完成值走 promise
送回，形状是 `{value: 完成值}`——**不隐式 await**，`Promise.resolve(7)` 给的是
`{value: <那个 promise>}` 而不是 `{value: 7}`。三种情况要分开：语法错**同步抛**（promise
还不存在），运行时错 reject，成功则 resolve `{value}`。

**`backtrace_barrier` 截断栈。** 参考用它让 REPL 只报刚求值的那行、不把 readline 循环也
报出来。实现为 `trace_barriers` 栈：记录建屏障时的字节码帧深度，出栈时只保留更深的帧。
屏障必须在**编译前**建立——语法错的栈也要截断。

**native 帧要排在它所属的字节码帧之间。** 原来 `stack_trace` 把所有 native 帧放在最前，
理由是"native 从不重入 JavaScript"。`std.evalScript` 重入了，这个前提不再成立，于是
`evalScript (native)` 跑到了它执行的代码上面。`native_frames` 现在记录入栈时的字节码深度，
`stack_trace_skipping` 按深度交错。顺带把 `skip` 的语义修对了——它原先分别数两种帧，
合并后才是"最内层 N 帧"。

### 13.3 顺带修掉的两个真 bug

**`std.__printObject` 多了一个换行。** 参考的 `js_std_file_printObject` 就是
`JS_PrintValue(ctx, js_print_value_write, stdout, …)`，不追加。`repl.js` 每条结果自己再
`std.puts("\n")`，所以我们这多出来的换行表现为**每条结果后一个空行**。既有测试
`print_object_adds_a_newline` 断言的正是错的行为，已按参考改正并改名。

**引擎内部抛的错误没有内部类。** `null.x` 的 `TypeError` 打印成 `{ stack: … }`，而
`new TypeError(…)` 打印成 `TypeError: …`——因为 `make_error` 没设 `_class`，只有构造函数
路径设了。为此把 `internal_slots` 改成 `RefCell`：驻留是**记忆化**不是可观察状态，用不可变
借用填缓存是细节，这样 `internal_set` 能在 `&self` 下工作，`make_error` 不必要求独占借用。

### 13.4 与参考的差异

`qjs -i < file` 的输入：

| | 参考 | 这里 |
|---|---|---|
| 结果输出 | `2` / `undefined` | 同 |
| 语法错栈 | `at <evalScript>:1:9` + barrier | 同 |
| 语法错措辞 | `invalid number literal` | `invalid source: 1:9: invalid number literal` |
| 管道 stdin 到 EOF | **永久挂死**（跑满超时，CPU 100%） | 干净退出 |

前两项是**纯文本共享**的多行输入副作用——第 2 行开始缓冲区里还留着第 1 行，参考同样如此
（`1:4`、`1:9` 两个报错位置两边一致）。第三项是缺口，见 §13.5。第四项是**刻意的**：参考在
EOF 上挂死，我们借 `stdin_eof()` 在"stdin 到 EOF 且没有定时器/微任务"时退出——复刻一个挂死
不是值得保留的行为。

### 13.5 还没做

**语法错消息措辞。** 参考是 `variable name expected`，我们是
`invalid source: 1:5: expecting a variable name, got '='`：多一个 `invalid source: L:C: `
前缀（M4 引入），且措辞本身不同。现有的 `parse_oracle.rs` 只对拍**接受/拒绝**，不验文本，
所以这件事完全没被跟踪。要对齐得逐条搬 QuickJS 的 `js_parse_error` 串，属独立一轮。

**顶层 `await`。** 已完成，见 §15。

**`std.evalScript` 的 `load`/`compile_only` 等其余选项**未实现，脚本触到时报错而非静默。

### 13.6 验收

- `cargo test` 全绿（新增 `std_eval` 6 项、`run.rs` 2 项、`disasm_diff` 1 组 6 例）
- `cargo clippy -D warnings` / `cargo fmt` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 18/18、NFR-6/NFR-7
- test262 `built-ins` 14907/17822 零变化；`language` 14941→14945（+4，-0）
- `qjs -i` 跑参考的 `repl.js`，结果输出与参考逐行一致

---

## 14. 语法错消息措辞对齐

### 14.1 先量化，再动手

`parse_oracle.rs` 只对拍**接受/拒绝**，不验文本——所以措辞完全没被测量。用
`test262/test/language` 全量跑了一遍（每个文件按它自己的 script/module 模式解析）：

| | 对齐率 |
|---|---|
| 动手前 | **2.3%**（93/4055） |
| 对齐后 | **44.5%** |

动手前有 408 类不同形状。全部是我们自创的字符串：参考说 `variable name expected`，我们说
`expecting a variable name, got '='`；参考说 `unexpected token in expression: ';'`，我们说
`unexpected token: ';'`。

### 14.2 参考的两条通用消息

搞清楚这两条是大部分工作的前提：

**`unexpected token in expression: '%.*s'`** —— 参考只有**这一条**通用 token 消息，而且
`%.*s` 是 token 的**原始源码切片**（`s->buf_ptr - s->token.ptr`），不是描述。所以文件结束
打印成 `''`，字符串字面量连着引号一起打印。我们原来是 `unexpected token: <描述>`，两者都不同。

**`expecting '%c'`** —— `js_parse_expect` 用 `%c` 格式化期望的 token，所以只能表达单字符
标点。我们的 `expect_punct` 站点恰好全是单字符（`( ) { } [ ] ; :`），因此可以纯机械对齐。

顺带把 `unexpected()` 的语义收紧：它原来在两种意图下混用（"期望某个标点"和"期望一个主表达式"）。
`expect_punct` 现在走 `expecting '<c>'`，`unexpected()` 只剩后者。

### 14.3 对齐的消息

机械措辞（纯字符串，无逻辑变更）：`variable name expected`、`function name expected`、
`class statement requires a name`、`invalid property name`（对象字面量／类成员）与
`expecting field name`（`.` 之后，参考是两条不同的消息）、`expecting target`、
`missing initializer for const variable`、`invalid number of arguments for getter or setter`、
`return not in a function`、`break must be inside loop or switch`、
`continue must be inside loop`、`invalid method name`、`private class field is already defined`、
`invalid number literal`、`unexpected end of string`／`end of regexp`、
`malformed escape sequence in string literal`、`invalid keyword: with`、
`cannot delete a direct reference in strict mode`、`duplicate __proto__ property name`、
`duplicate argument names not allowed in this context`、`undefined private field '#x'`、
`invalid variable name in strict mode`、
`"use strict" not allowed in function with default or destructuring parameter`、
`cannot mix ?? with && or ||`、`invalid increment/decrement operand`、
`missing formal parameter`、`identifier or string expected`、`string expected`、
`unparenthesized unary expression can't appear on the left-hand side of '**'`。

### 14.4 三处需要改判定，不只是改字符串

**重定义的四条消息。** 参考按冲突绑定**在哪里**分四种：同作用域的 lexical →
`invalid redefinition of lexical identifier`；顶层 `var` → `... of global identifier`；
函数内的 `var`/函数声明 → `... of a variable`；同体的简单形参 →
`... of parameter name`。20 个探针确认了边界（包括 `{ { var x; let x; } }` 是 global 而
`function f(){ { var x; let x; } }` 是 variable）。实现为 `Redefinition` 枚举 + 
`at_top_level_scope()`。

顺带发现 catch 参数被塞进了 `params` 字段，于是 `try{}catch(e){let e;}` 会报
`parameter name`——参考走 lexical 分支。拆出独立的 `catch_names` 字段，同时保住
`catch (e) { var e; }` 的豁免。

**赋值目标的两条消息。** `1 = 2` 是 `invalid assignment left-hand side`，`[1] = 2` 是
`invalid destructuring target`——区别就是"是否在对象/数组字面量被当作模式重解释的过程中"，
即函数里已有的 `nested` 标志。`++1` 又是第三条：`invalid increment/decrement operand`。

**`super` 的三条消息。** 裸 `super` → `invalid use of 'super'`；`super.x`／`super[0]` →
`'super' is only valid in a method`；`super()` →
`super() is only valid in a derived class constructor`。按 `super` 后面跟什么区分。

### 14.5 测出来的东西（不止措辞）

差分测试在写的过程中抓出三处我原本没注意的分歧：

- `class A {` 报 `unexpected token in expression: ''` —— 类体循环里有个过早的 EOF 守卫，
  挡在"期望属性名"之前。去掉后 `class A {`、`class A { x` 都给出参考的
  `invalid property name`，而 `class A { [`、`class A { m() {` 仍然是
  `unexpected token in expression`，与参考一致。
- `function f(` / `function f(1)` / `function f(...)` 报 `variable name expected` —— 参考说
  `missing formal parameter`。`function f(...a,)` 则是 `expecting ')'`（rest 参数后
  只能跟 `)`）。
- 未声明的私有字段：`this.#x` 参考只说 `undefined private field '#x'`，不解释"必须在封闭类
  中声明"。

### 14.6 刻意的差异

**`do {}`（缺 `while`）**：参考输出 `expecting '<U+FFFD>'`。`js_parse_expect` 用 `%c` 打印
期望的 token，而 `TOK_WHILE` 装不进一个字节，于是消息里是一个**非法 UTF-8 字节**。参考自己的
源码在那行上方就写着 `XXX: dump token correctly in all cases`。复刻损坏的输出不值得。
这条在测试里单独标为 `UNPINNABLE`——参与比较，但不钉住参考文本。

### 14.7 还没做

- **两套重名消息**：参考有 `duplicate parameter names`（`js_parse_check_duplicate_parameter`）
  和 `duplicate argument names`（`js_parse_function_args`）两条，走哪条取决于哪条扫描路径先
  撞上。实测 `function f(a, a = 1)` 和 `function f(a, [a])` 是前者，
  `function f({a}, a)` 和 `function f(a,a){"use strict"}` 是后者。要对齐得镜像它的两次扫描。
- **rest 元素的 `= ` 家族**：`[...a = 1] = b` 参考说 `rest element must be the last one`，
  `[...{a} = 1] = b` 说 `rest element cannot have a default value`。同样是路径依赖，试改一次
  没触发，已撤回。
- **`await` / `yield` 上下文**：`async function f(){ await = 1 }` 参考报的是
  `unexpected token in expression: '='`（即它把 `await` 当普通标识符走到更后面），我们在更早
  的位置报 `unexpected 'await' identifier in an async function`。诊断顺序差异。
- **接受性缺口 3 处**（不是措辞）：未 tagged 模板里的非法转义我们接受、`class A { get
  constructor(){} }` 我们接受、`constructor(){ super; }` 我们接受。都在测试里登记。

### 14.8 新增的测试

`crates/quickjs-parse/tests/parse_wording.rs`，5 个测试：

- `syntax_error_wording_matches_the_reference` —— 87 条语料，逐条比对两边的消息。
  没登记的差异**失败**，登记的差异**仍然差异**（开始一致也失败，逼着列表缩短）。
- `module_error_wording_matches_the_reference` —— 模块模式的单独一组。
- `the_recorded_expectations_still_match_the_reference` —— 语料里钉的期望文本必须仍是参考的；
  参考更新导致改词时，失败的是这条而不是差分测试静默通过。
- `every_declared_difference_is_actually_run` —— 登记项必须有语料，否则理由会变成注释。
- `the_corpus_has_no_duplicates` —— 防止复制粘贴让一个用例藏在它的孪生后面。

两张登记表分开：`KNOWN_DIFFERENCES`（措辞，改一个字符串的活）与
`KNOWN_ACCEPTANCE_GAPS`（一方接受一方拒绝，缺一个检查的活）。

### 14.9 验收

- `cargo test` 全绿（新增 `parse_wording` 5 项）；`clippy -D warnings`／`fmt` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 18/18、NFR-6/7
- test262：`built-ins` 14907/17822 **零变化**；`language` +4 **零回归**
- 措辞对齐率 2.3% → 44.5%

---

## 15. 顶层 `await`（可挂起脚本）

### 15.1 参考只做了一件事

参考的相关代码只有一行（`quickjs.c:38570`）：

```c
if (m != NULL || (flags & JS_EVAL_FLAG_ASYNC)) {
    fd->in_function_body = TRUE;
    fd->func_kind = JS_FUNC_ASYNC;
}
```

所以**模块体**和 `evalScript(…, {async:true})` 走的是同一条路：程序自己的函数是 async。
`await` 的所有规则都从 `fd->func_kind & JS_FUNC_ASYNC` 派生，没有第二套开关。

把这个对应到我们已经有的结构上，就是一个字段：`FnCtx::in_async`。它本来就描述"当前
正在解析的函数"，所以 `Parser::new` 里 `in_async = module`，可挂起脚本再置 true 即可。
原来分散在四处的 `(self.module && self.top_level)` 判定随之全部收敛成 `self.ctx.in_async`。

注意它**不是** `in_function`：`return 1` 与 `new.target` 在可挂起脚本顶层仍然报错，与普通
脚本完全一样。这两条有单独的测试（`the_flag_does_not_make_the_body_a_function`），因为
"让 await 合法"和"变成函数"很容易被混为一谈。

### 15.2 编译端：`{value}` 包装

参考在 `js_parse_program` 收尾时，对 `!is_module && func_kind == JS_FUNC_ASYNC` 的脚本发出：

```
OP_object
OP_dup
OP_get_loc <ret>
OP_put_field value
OP_return
```

（源码注释：*"wrap the return value in an object so that promises can be safely returned"*）。
没有这层包装，用完成值去 resolve 一个 promise 会**采用**它：`evalScript("Promise.resolve(7)")`
会得到 `7` 而不是 `{value: <promise>}`，只有显式 `await` 才解包。这与 REPL 打印 promise 而
非其值是同一条规则。

`compile_script_suspensible` 除此之外与 `compile_script` 一致（脚本仍然报完成值），只是
`kind = Async`。字节码形状在 `suspensible_script.rs` 里按指令序列钉住。

### 15.3 运行时：body 变协程

`run_eval_script` 末尾原本直接 `execute`；现在 async 体走 `run_async_body`，返回 promise
而不是值。`this` 必须继续传（全局 eval 的 `this` 是全局对象，`this === globalThis` 为 true）。
`eval_in_global_async` 也不再自己包一层 promise——async 体产出的那个就是参考返回的那个。

### 15.4 `await` 的三种上下文

这一轮把 `await` 的位置规则按参考的模型重排了。参考的模型是**两层**：

1. **是不是关键字**：`in_async || module`。所以模块里**任意深度**（包括普通函数体内）
   `await` 都不是标识符——`function f(){ var await = 1 }` 在模块里是语法错，在脚本里不是。
2. **能不能当算符**：只有 `in_async`。模块的普通函数体内它是关键字但不能 await，参考为此
   专门有一条 `unexpected 'await' keyword`。

顺手修掉的三处（都是探针发现的，不是猜的）：

- **`await` 的"看下一个 token 再说"启发式是错的。** 原来 `await;`、`f(await)`、`await .x`
  会退回成标识符引用，因为启发式把 `)` `]` `}` `,` `;` `:` `=` `.` `[` 当作"不可能开始
  表达式"。参考在 `TOK_AWAIT` 上无条件 `next_token` + `js_parse_unary`，所以这些一律报
  *操作数* 的错（`unexpected token in expression: ';'`）。去掉启发式后 12 条探针全对。
- **`await: 1` 不是标签。** 标签路径原本先抢下 `await` 再报 `variable name expected`。
  在算符位置上应当落到表达式路径；只在"仅因 module 而保留"的那一侧才报
  `unexpected 'await' keyword`。
- **`function f(await){}` 在模块里是 `missing formal parameter`**，不是 `variable name
  expected`：参考根本没见过 `await` 作为标识符 token，所以走的是"形参缺失"分支。

`await_contexts.rs` 把三种上下文 × 30 条源码（87 项检查）与参考逐条比对。

### 15.5 顺带修掉的运行时消息

探针顺带暴露了三条**运行时抛出**的语法错与参考不同（都属于上一轮"措辞"那一族）：

| 场景 | 参考 `e.message` | 修前 |
|---|---|---|
| `eval("1 +")` | `unexpected token in expression: ''` | `invalid source: 1:4: unexpected token in expression: ''` |
| `new Function("1 +")` | `unexpected token in expression: '}'` | `invalid function body: 1:17: …` |
| `JSON.parse("{]")` | `expecting property name` | `expected a string key` |

`invalid source: L:C: ` 前缀是 M4 留下的：位置本来已经写在 `fileName`/`lineNumber`/
`columnNumber` 和栈帧上，参考的 `message` 是裸的。去掉后 `e.message` 与参考一致，
`e.stack` 早就一致。

### 15.6 发现但未修（与本轮无关，先登记）

- **`arguments` 在全局 eval 里存在。** `std.evalScript("arguments")` 与
  `evalScript` 体内的 `eval("arguments")`，参考都抛 `'arguments' is not defined`，我们在
  调用者帧存在时给出一个空的类数组。正确做法是把 `arguments` 建模成调用者变量环境里的
  普通绑定（`eval(b)` 把被调者当值传，连 callsite 探测也答不出来）。比这一轮大，已留注释。
- **模块顶层的 `using` 会让编译器 panic**（`a using binding was declared during hoisting`）。
  这是 panic 不是错误消息，应当单独修；`await using` 在模块顶层正常。
- **`new Function` 的语法错位置**：参考报 `fileName = <input>`、`lineNumber = 4`，并在栈里
  有 `at <input>:4:1`；我们没有任何位置。原因是两边生成的包装源码布局不同，报我们自己的
  位置会更糟，所以只对齐了 message。
- **`a = { await }`**：参考 `expecting ':'`，我们 `unexpected 'await' identifier…`。对象字面量
  简写路径的问题，与 `await` 本身无关。

### 15.7 新增的测试

- `crates/quickjs-parse/tests/await_contexts.rs` —— 3 种上下文 × 30 条源码 = 87 项检查，
  全部差分参考。可挂起脚本这一列通过 `qjs --std` 跑一个 `evalScript(source, {async:true})`
  的探针取得（`qjsc` 只有 `--script`/`--module`，没有 async eval 入口）。
  另有 `the_flag_does_not_make_the_body_a_function`：`return`/`new.target`/`super()` 在
  加 flag 前后必须报同一条消息。
- `crates/quickjs-compile/tests/suspensible_script.rs` —— 4 项：kind 是 async、
  收尾五条指令 `object; dup; get_loc0; put_field; return` 的序列、普通脚本不带包装、
  `await` 编译成 `await` 指令。
- `crates/quickjs-host/tests/std_eval.rs` +5 项（共 11）：顶层 `await` 合法、promise 不被
  采用而是被包装、全局 receiver 与全局 `var`、`await` 真让出事件循环并与其它 job 的
  先后顺序一致、无 flag 时非法。
- `parse_wording.rs` 去掉了 `async function f() { await = 1 }` 这条登记——本轮改动让它
  自动对齐了（差分测试自己报的"登记的差异已一致"）。

### 15.8 验收

- `cargo test` 全绿（新增 2 个测试文件、`std_eval` +5）
- `cargo clippy -D warnings` / `cargo fmt` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 18/18、NFR-6/7
- test262：`built-ins` 14907/17822 零变化；`language` 14945→14972（**+27，-0**），
  含 `module-code/top-level-await/*` 与 `class-name-ident-await-module` 等

---

## 16. CLI 的未捕获错误输出

三件事，各自错在不同地方。判据都是 `qjs <file>` 的 stderr 逐字节对拍。

### 16.1 抛出的值要用打印器，不是 `ToString`

参考报未捕获异常走 `js_std_dump_error`，它三行、最后一行是
`JS_PrintValue(ctx, …, stderr)` —— 就是 `std.__printObject` 背后那个打印器。我们用的
是 `ToString`，于是：

| `throw` | 参考 | 修前 |
|---|---|---|
| `"s"` | `"s"` | `s` |
| `{a:1}` | `{ a: 1 }` | `[object Object]` |
| `[1,2]` | `[ 1, 2 ]` | `1,2` |
| `Symbol("y")` | `Symbol("y")` | `Symbol(y)` |
| `new Error("b")` | `Error: b` + 栈 | `Error: b` |

15 条探针（含循环引用、`Map`/`Set`、函数、`BigInt`）现在全部一致。

顺带解决了**打印器该住在哪**这个问题：参考的 `JS_PrintValue` 在**引擎**（`quickjs.c`）
里，host 只是包了 `std.__printObject`。所以 `print.rs` 从 `quickjs-host` 搬到了
`quickjs-pure`（它本来就只依赖引擎，NFR-7 的"依赖闭包为空"不受影响）。这样
`describe_exception` 才能用它 —— 之前留在 host 里，是因为只有 `std` 需要它。

### 16.2 语法错：消息在前，位置单独一行

参考：

```
SyntaxError: variable name expected
    at c.js:1:5
```

我们：

```
c.js: SyntaxError: 1:5: variable name expected
```

文件名在参考不放东西的位置，位置在参考放文件名的位置。`invalid source: L:C:` 前缀是
M4 留下的（§13.5 已登记），本轮一并去掉 —— 位置本来就已写在 `fileName`/
`lineNumber`/`columnNumber` 和栈帧上。

修在 `Error::syntax`：`SyntaxError: <message>` 加（有位置时）`    at <file>:<line>:<col>`。
CLI 不再加 `{filename}: ` 前缀。

### 16.3 有一类错误参考不给位置

`export {a}` 而不存在 `a`：参考输出 `SyntaxError: exported variable 'a' does not exist`
——**没有** `at` 行，也没有末尾空行之外的东西。因为它在 `js_resolve_module` 里报，解析
早已成功。我们是在解析器里做这个检查（手上正好有已声明名字的集合），所以得显式说明
"按链接错误报"：`SyntaxError` 新增 `positionless` 字段与 `.positionless()` 构造器。

### 16.4 空栈仍要换行

`print_error` 原来按"栈非空"决定要不要换行，参考按 `JS_IsString(stack)`。两者只在
**空字符串**上不同 —— 而无 JS 帧时建的错误（模块链接错误就是）正好栈是空串。于是参考
打两行（`SyntaxError: msg` + 空行），我们打一行。改为"是不是字符串"后一致。

### 16.5 顺带修掉的真 bug

探针暴露的、都不是本轮引入的：

- **`new Proxy({}, 1)` 我们接受**。参考一律 `TypeError: not an object`。原实现只拒
  `null`，数字/字符串/`undefined` 全放行。且措辞从
  `proxy target must be an object`/`proxy handler must be an object` 改为参考的单条
  `not an object`（参考不说哪个参数错了）。
- **未声明名字的 `ReferenceError` 少引号**：参考 `'x' is not defined`，我们 `x is not defined`。
- **`BigInt(1.5)` 与 `BigInt(NaN)` 共用一条消息**：参考分开成
  `cannot convert to BigInt: not an integer` 与 `cannot convert NaN or Infinity to BigInt`。
- **TypedArray 窗口的错误分类**：`invalid offset` 与 `invalid length` 不是同一分支。
  参考在没有第三参数时查 `offset > byteLength`（`invalid offset`）以及整 buffer 长度能否
  被元素大小整除（`invalid length`）；有第三参数时只查 `offset + len*size > byteLength`
  （`invalid length`）。原来合并成一个 `||`，导致四种情况错三种。
- **`using` 直接写在 `case`/`default` 的语句表里**要报
  `using declaration is not allowed in this context`。这一条是我上一轮把顶层检查从
  `top_level` 改成"作用域深度 == 1"时放出来的回归 —— 块内的 `using` 应当放行，Clause 的
  语句表里不行。新增 `in_case_clause` 标志，并在进入块/函数体时清掉。

### 16.6 测试

- `crates/quickjs-cli/tests/uncaught.rs`（5 项）：抛出的值是打印而非字符串化（16 条）、
  错误带栈（11 条，含子类 `name`、自定义 `toString`、未处理 rejection）、语法错带位置
  （7 条）、`-e` 下命名为 `<cmdline>`（4 条）、模块链接错误无位置。全部走
  `Case::script`/`Case::eval` 与参考二进制逐字节比对，stderr 也算。
- `crates/quickjs-pure/tests/builtins.rs` 与 `proxy.rs`：11 处期望补上栈行。这些期望原先
  钉的是"无栈"，即我们旧行为的记录，不是参考的输出。其中 3 处参考与我们仍不同（
  `new BigInt(1)` 少一个 native 帧、`eval('super.x')` 位置差 5 列、`using` 的 disposer
  帧名与位置），按我们的实际输出钉住并注明缺口。

### 16.7 test262 运行器：负例改按抛出值判定

这一轮让 11 个 `line-terminators/comment-*` 从通过变成"参考也过不了"：它们
`throw new Test262Error()`，而参考把普通对象打成 `{ message: "" }` —— `Test262Error`
一个字都不出现。运行器原先用 `stderr.contains("Test262Error")` 判定，靠的正是我们那条
（错误的）`ToString`。

test262 本身不是这样判的：`run-test262.c` 捕获异常值，比较它的构造器名。所以运行器改为
对 `negative: runtime` 的用例包一层 `try`/`catch`，打一行
`Test262:Threw:<构造器名>`，判定时优先看这行。只对非 `raw` 的脚本用例生效（模块会把
源文本变成非模块，`raw` 没有 harness 保证它是脚本）；这类用例整体预期会抛，额外的块作用域
观察不到。

跑完后 `statements/using` 里暴露出真 bug 4 个（§16.5 最后一条），修完
`language` 净 +15 / -0。

### 16.8 发现但未修

- **`new BigInt(1)` 少一个 `at BigInt (native)` 帧**：我们在进入 native 构造器之前就拒了。
- **`using` 的 disposer 帧**：参考 `at <anonymous> (f.js:1:35)`，我们 `at  (f.js:1:26)`
  —— 函数名空、位置指向声明而非释放点。
- **`eval('super.x')` 的位置**：参考 `1:6`，我们 `1:1`。
- **`class A { static { … } }`**：编译器曾不支持静态块（`static blocks are not supported
  by the compiler yet`）。**已在 §18 实现**。
- **模块顶层 `using` 会让编译器 panic**（§15.6 已记，仍在）。
- **一个偶发失败**：`TypedArray/prototype/copyWithin/coerced-values-end-detached-prototype.js`
  在一次完整 `built-ins` 跑里 FAIL，之后单独跑 6 次、完整跑 2 次都 PASS。该用例分配
  10000 元素数组并分离 buffer，并行 8 个 job 下有地址空间/时序压力。目前不接受为已知失败，
  只记录：它不稳定。

### 16.9 验收

- `cargo test` 全绿（新增 `uncaught.rs` 5 项）
- `cargo clippy -D warnings` / `cargo fmt` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 18/18、NFR-6/7
- test262：`language` 14972→14987（**+15，-0**）；`built-ins` 14907→14914（**+7，-0**）

---

## 17. 早期错误：重复声明与重复参数

这两族 early error 有一个共同的危险性质：**接受一个本应拒绝的程序是静默的**。多一个绑定、
少一条报错，在这个程序跑起来之前没有任何东西会抱怨；等它有影响时，症状离原因已经很远
（`{ function f(){} var f; }` 我们接受，然后 `f` 到底是函数还是 `var` 就取决于猜）。

所以这一轮又是同一个顺序：先把判定做成可执行的差分，再改代码。

### 17.1 差分先立起来

`crates/quickjs-parse/tests/redeclaration.rs`，两张表：

| 表 | 条数 | 比什么 | 判据 |
|---|---|---|---|
| `REDECLARATIONS` | 41 | 消息**与位置**（脚本 + 模块各一遍 = 82 项） | `qjsc -c` 的 `SyntaxError` 首行 + `at case.js:L:C` 行 |
| `DUPLICATE_PARAMETERS` | 24 | 消息**与位置** | 同上 |

位置也是逐项比的。参考的 `js_parse_error` 一律用 `s->token.ptr`——**当前 token**——所以
"位置"其实只是"检查发生在哪个 token 上"的直接后果，而两族的答案不同：

- **重复声明**：检查在名字刚被消费之后做（`js_define_var`），所以报在"名字之后的那个
  token"。`{ let a; let a = 1; }` 报在 `=`，`{ let a; let a; }` 报在 `;`。
- **重复参数**：有**两处**检查，位置各不相同。解析期的早检查（`js_parse_destructuring_var`
  在 `next_token` 之前调 `js_parse_check_duplicate_parameter`）就报在名字**本身**；收尾
  检查则已经在函数体里，报在**体的第一个 token**——空体的 `}`、`{ return 1 }` 的
  `return`、`(a, a) => 0` 的 `0`。

后者一开始被登记成"已知缺口"，理由是要抄参考的 `SKIP_HAS_ASSIGNMENT` 前瞻才能定位。实测
发现不是：真正要抄的是"收尾检查在 `{` 之后才跑"这一条，前瞻只决定**报哪条消息**。于是改法
只是给 `check_duplicate_parameters` 多传一个位置：`parse_function_body` 取 `{` 之后的第一个
token，箭头分支取 `=>` 之后的第一个（若跟的是块，再往后一个）。

改前的账（同一组用例，逐项跑过）：

| | 改前不一致 | 分类 |
|---|---|---|
| 重复声明 82 项 | **63** | 36 项只是位置差、21 项我们**接受了非法程序**、6 项消息不同 |
| 重复参数 24 项 | **18** | 4 项我们**拒了合法程序**（sloppy 的 generator / `async function`）、4 项该查的方法/箭头没查、10 项消息不同 |

改后两表全对。参数族当时只比了消息；把位置也纳入比较之后又暴露出"收尾检查指在名字上"这一
处（见 17.1），补完才两族都逐字节对齐。

### 17.2 位置不是"挪一下消息"，是"在对的地方做检查"

36 项位置差全部来自一处：我们原来是**解析完整个声明符之后**才登记名字，参考是**读到名字就
登记**（`js_parse_var` 的 `set_val` 调 `js_define_var`，此时当前 token 已经是 `=`）。
于是 `{ let a; let a = 1; }` 参考报在 `=`（1:16），我们报在名字（1:15）。

改法是把 `declare_pattern` 搬到初始化器之前。这不是搬位置，而是搬判定点——初始化器里若有
别的错，参考也是先报重复声明。

`class` 与块级函数声明的偏差更大（`{ let a; class a{} }`：参考 1:20 的 `{`，我们 1:16），
同样是"检查发生在名字之后"带来的。

### 17.3 块里的函数声明是词法的，函数体里的像 `var`

参考用一处 `define_var` 分叉表达这件事，判据是 `fd->scope_level == fd->body_scope`：

| 位置 | 形态 | 例子 |
|---|---|---|
| 块（`scope_level != body_scope`） | 词法绑定 | `{ function a(){} var a; }` → `invalid redefinition of lexical identifier` |
| 函数体/脚本/模块体 | 像 `var` | `function f(){ function a(){} var a; }` → 合法 |

我们本来只有后半：`declare_function` 只在最后登记 `var_names`，块内的函数声明**什么都没记**。
这就是 21 项"我们接受了非法程序"的来源——`{ function a(){} var a; }`、
`{ function a(){} function* a(){} }` 等等。

现在按参考拆成两半，各自在参考做检查的时刻执行：

- `declare_block_function`——从 `parse_function` 里、名字刚读到时调用；只在**块**作用域做事，
  并把名字记进 `lex_names`/`fn_names`。
- `declare_function`——从语句的调用点、整条函数解析完之后调用；只在**体**作用域做事。

两半各做各的，所以 `function a(` 的冲突报在 `(`、体级冲突报在函数末尾，与参考一致。

**Annex B.3.3.4 只覆盖 plain function**：sloppy 下 `{ function a(){} function a(){} }` 合法，
但 `{ function a(){} function* a(){} }` 不合法——参考要求 `var_def_type == JS_VAR_DEF_FUNCTION_DECL`，
而 generator / `async function` 用的是 `JS_VAR_DEF_NEW_FUNCTION_DECL`。所以多了一张
`Scope::plain_fn_names`，记录"plain 的那些"。

### 17.4 三处作用域记账，各不相同

改的过程中撞上三件不能用一张表表达的事：

- **`var` 的名字要记两次。** `var_names` 是**传递**的（`{ var a; }` 把 `a` 放进块**和**外层
  函数，词法冲突检查要的就是这个集合）；模块的函数声明检查却按作用域**层级**比：块里的
  `var` 只与**同一个块**里的函数冲突，与模块顶层的函数不冲突。于是新增非传递的
  `var_names_here`。
- **`import` 是词法的，但不是模块变量。** 参考的 `find_global_var` 看不到 import，所以在模块里
  `import {a} from "x"; function a(){}` **不**报错。我们的 `lex_names` 里 import 与 `let` 混在一起，
  只能再加一张 `import_names` 把它挑出来。
- **脚本顶层的函数声明不建词法名。** 参考的 `avoid creating a lexical variable in the global
  scope` 分支：体级的函数声明只进 global var 表，并只在表里**已有**同名项时报错。于是
  `let a; function a(){}`（脚本）**合法**，而 `function a(){} let a;` 报
  `invalid redefinition of global identifier`——方向是反的，很容易凭直觉写反。

对应地，模块的同类检查有自己的一条消息（`invalid redefinition of global identifier in module
code`），且只对**模块变量**生效：41 条里恰好 3 条在模块模式下走到它——`let a; function a(){}`、
`var a; function a(){}`、`{ var a; function a(){} }`（最后一条能命中，正因为那个 `var` 写在这个
块里，与函数同层）。模块顶层的 `let` **是**模块变量，所以前两条与脚本里的结论相反。

### 17.5 重复参数：两条消息，两个判定时刻

改前只有一个判定、一条消息，两个方向都错：

**什么时候非法**（参考 `js_parse_function_args`）：strict、列表不简单、箭头、方法。只有
"sloppy 简单列表的 `function f(a, a){}`"合法——而 `function* f(a, a){}` 与
`async function f(a, a){}` 也**合法**（参考的条件里没有 generator/async，只有 method）。
我们原来把 `|| is_generator || is_async` 也算进去了，于是拒了两个合法程序。

**用哪条消息**：参数解析期的早检查说 `duplicate parameter names not allowed in this context`，
收尾检查说 `duplicate argument names not allowed in this context`（无名字）。早检查只在两种
情况下才走到——列表里任何位置有 `=`，或名字在解构 pattern 里。这解释了表里成对的条目：
`function f(a, a){}` 说 `argument`，`function f(a, a = 1){}` 说 `parameter`。

还有一个顺序细节：`function f([a], a, [a]){}` 报的是**第三个**参数，因为第二个（plain，
且列表无 `=`）要等收尾检查，而第三个在读到时就被早检查抓了。

顺带去掉一处自造措辞：箭头函数的重复参数原本是
`duplicate argument names not allowed in this context ('a')`，参考不打印那个名字。

### 17.6 顺带修掉的两个真 bug

都是 16.8 登记的，本轮因为它们挡在同一段代码上而一并修掉：

- **模块顶层的 `using` 会让编译器 panic**（`a using binding was declared during hoisting`）。
  模块顶层声明走模块格（`declare_module`），而 `using` 的释放尾声按**槽位**寻址，两套地址
  机制不可能对上，于是 hoisting 时找不到槽直接 `expect` 炸掉。参考的 `declare` 排除
  `using`（`var_def_type != JS_VAR_DEF_USING`，注释 "using variables are currently not
  exported in modules"），于是模块顶层的 `using` 是体函数的普通局部——释放正常，但不能导出
  （`export { a }` 报 `exported variable 'a' does not exist`，按链接错误处理，无位置）。这两条
  各有测试（`module.rs`：正常释放、抛异常时也释放、不能导出）。
- **栈帧名 `<anonymous>`**：模块体函数对象是 `js_closure2` 建的，参考从没跑过
  `js_function_set_properties`，所以它**没有** `name` 属性；脚本体则有（`<eval>`）。原来我们在
  没有 callee 对象时打印编译期的 `code.name`，于是模块帧打了个名字出来。现在按"没有名字"
  打印 `<anonymous>`。`using` 的 disposer 帧名字也随之对上（位置仍差，见 17.7）。

### 17.7 发现但未修

- **`using` 的 disposer 帧位置**：参考指 `1:35`（调用 disposer 的地方），我们仍指声明处
  `1:26`。名字已经对上（`<anonymous>`）。
- 16.8 的其余项不受影响，仍在：`new BigInt(1)` 少一个 native 帧、`eval('super.x')` 的位置、那条不稳定的 TypedArray 用例。（`class A { static { … } }` 编译器不支持这一项已由 §18 修掉。）

### 17.8 验收

- `cargo test` 全绿（新增 `crates/quickjs-parse/tests/redeclaration.rs` 4 项、
  `module.rs` +3、`builtins.rs` 一处期望改名）
- `cargo clippy -D warnings` / `cargo fmt` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 21/21、NFR-6/7
- test262：`built-ins` 14914/17822 零变化；`language` 15064/23743 零变化（**+0，-0**）。
  这一轮改的是早期错误，而 test262 里对应的负向用例此前已被别的检查挡住（`block-scope/
  syntax/redeclaration` 98 条全 PASS，改前也是），所以数字不动是对的——不是没测到，
  而是这里本来就没有失败项。差分测试覆盖的是"参考与我们逐条一致"，比总数更细。

## 18. class 静态块（`static { … }`）

一句话概括：静态块**不是一个成员**，是一段在类定义走到它时立刻执行的语句。参考把它编成
一个函数、塞进静态字段初始化器里调用——所以"实现静态块"实际上是"让静态字段初始化器也能
装语句"，而这条线索牵出了四件事：静态块自己的作用域、它继承来的 `this`/`super`、
`super.x = v` 的赋值形态，以及一组只属于块内作用域的早期错误。

### 18.1 参考的字节码形状

`class C { static a = 1; static { this.b = 2 } static c = 3 }`：

```
function: <null>            ← 静态字段初始化器（1:1）
  locals: 0: var this
  opcodes:
        push_this / put_loc0 this
        get_loc0 this / push_1 / define_field a
        fclosure8 0             ← 静态块本身
        get_loc0 0: this
        swap
        call_method 0
        drop
        get_loc0 this / push_3 / define_field c
        return_undef

function: <null>            ← 静态块（`{` 的位置）
  closure vars: 0: var this [loc0]
  opcodes:
        get_var_ref0 0: this / push_2 2 / put_field b / return_undef
```

三处细节都不是随便选的：

1. **静态块和静态字段共用同一个初始化器**，所以两者按源码顺序交错。分成两个 pass 会让
   `static { this.a = 1 }; static a = 2` 与反过来的写法得到同一个结果，而参考里它们是不同的。
2. **静态块函数没有 `this`**：`has_this_binding = FALSE`，`this` 从初始化器闭包进来。
   这也是它为什么能是一个**普通函数**却表现得像箭头——它没有自己的 `this`、`new.target`、
   `arguments`，三者全部向上取。
3. **`super` 的 home object 也从初始化器来**（`has_home_object = FALSE`，
   `super_allowed = TRUE`）：块自己身上没有 `[[HomeObject]]`，读到的是初始化器的。
   初始化器有（`set_home_object` 由 `call_method` 之前那条指令放的），于是 `super.x` 落在
   父类的**静态**那一侧。

到此为止都是照抄形状，真正的坑在下面两条。

### 18.2 我们这边：`super` 在 arrow 里从来就是错的

照第 3 条写代码时发现，`super_base()` 一律发 `special_object 4`（当前函数的 home object）。
对方法没问题，对**箭头**是错的——箭头没有 home object，压出来是 `null`，
`get_super` 拿到 `null` 的 prototype，于是：

```js
class B { m(){ return 5 } }
class C extends B { n(){ return () => super.m() } }
new C().n()          // TypeError: cannot read property 'm' of null
```

这个问题在静态块之前就存在，只是没人写过"方法里套箭头再 `super`"。参考的机制是
`resolve_pseudo_var`：没有 `has_this_binding` 的函数拿不到 `<home_object>` 局部，名字按普通
变量往外找，最终落到最近一个有 home object 的祖先，由它在函数入口 `special_object 4` +
`put_loc` 填一次。

这里照这个机制补了 `needs_home_object_local`，和 `this`/`new.target` 完全同构
（`provide_*_local` + `prepend_initialiser`）。副产物是上面那段代码现在返回 `5`。

### 18.3 我们这边：`get_super_value` 多留了一个槽

顺手对拍 `print(super.p)` 时炸出 `TypeError: class constructors must be invoked with 'new'`，
栈顶第一帧是**类的构造函数**。原因不在静态块：

```rust
// 改前
let key = self.pop();
let proto = self.pop();
let receiver = self.peek(0);   // ← 读，没消费
self.push(value);              // 于是 this 留在了栈上
```

`get_super_value` 的栈效应是 **3 进 1 出**（参考 `sp[-3] = val; sp -= 2`），我们做成了
3 进 2 出。这个泄漏之所以长期没被发现，是因为它只在"结果被消费"的位置可见：

- `return super.x` / `super.x;`：结果在上面，多出来的 `this` 在下面，谁也碰不到；
- `print(super.x)`：调用指令按 `func, this, args…` 找，于是 `this`（也就是类）被当成函数调了。

改成 `pop` 之后 `print(super.p)` 与参考一致。**这是本轮最值钱的一处**：静默的栈错位。

### 18.4 `super.x = v`：三进四出，`insert4` 复制一份

在此之前 `super.x = 1` 直接报 `this expression form is not supported by the compiler yet`
（`assign` 对 `PatternKind::Member` 的 object 调 `expr`，而 `SuperProp` 不是能求值的表达式）。
参考的写法是先把四个值一起凑齐再交给 `put_super_value`：

| 形态 | 指令 |
|---|---|
| `super.a = v` | `this home get_super a v insert4 put_super_value` |
| `super[k] = v` | 同上，键不做 `to_propkey`（由 `put_super_value` 自己转一次） |
| `super.a += v` | `… to_propkey dup3 get_super_value v add insert4 put_super_value` |
| `super.a \|\|= v` | `… to_propkey dup3 get_super_value dup if_true L drop v insert4 put_super_value goto E L: nip nip nip E:` |

`insert4` 的语义是 `this obj prop a -> a this obj prop a`——**复制**一份 a 放到栈底再转一圈，
所以 `put_super_value` 吃掉四个之后还留着赋值表达式的值。运行时里 `insert4`/`dup3` 此前都没实现
（`dup3 is not implemented yet`），一并补上。

有一处细节值得记：既然值留在了栈上，语句意义上那个 `drop` **不能**由 `assign_super` 自己发。
第一次就是多发了一条——`insert4` 保住了值，`assign` 又发一条 `drop`，表达式语句再发一条，
于是每条 `super.a = 1;` 后面跟两个 `drop`。正确的分工是跟普通成员存储相反的那一侧：
`put_field` 消费值（置 `no_value`），`put_super_value` 保留值（什么都不做），由语句统一收尾。

### 18.5 早期错误：块是块，不是函数

参考在这条路径上有两个开关：`func_type == JS_PARSE_FUNC_CLASS_STATIC_INIT`，以及
`has_this_binding/has_arguments_binding = FALSE`。落到语法上：

| 写法 | 结果 | 为什么 |
|---|---|---|
| `return` | `return in a static initializer block` | 写在块里，没有函数可返回 |
| `arguments` | `'arguments' identifier is not allowed in class field initializer` | 继承自初始化器的禁令 |
| `await x` / 裸 `await` | `unexpected 'await' keyword` | 块不是 async，但 `await` 在这里是保留字 |
| `super()` | `super() is only valid in a derived class constructor` | 块不是构造函数 |
| `super.x` | 合法 | home object 是类 |
| `new.target` | 合法，值为 `undefined` | 块是被 `call_method` 调的，不是 `new` 出来的 |
| `var a` | **合法**，是块函数的局部 | 参考不实现 spec 里那条禁令（见 18.8） |
| `yield` | `unexpected 'yield' keyword` | 块是 strict，`yield` 是保留字 |

`await` 这条要说清楚，因为它的判定位置很反直觉：`update_token_ident` 里
`func_has_ancestor_kind` 对 `CLASS_STATIC_INIT` 特殊返回 TRUE，所以 `await` 被转成关键字，
到了 `js_parse_unary` 又因为 `func_kind` 不是 ASYNC 而报"unexpected"。而那个"向上找祖先"
的循环在**箭头函数体**那一层就断了，所以：

```js
class C { static { await } }          // SyntaxError
class C { static { () => await } }    // 合法：await 只是普通标识符
```

这两条都在差分测试里。

`arguments` 这条顺带修了一个早就存在的洞：**实例字段初始值里**的 `arguments` 我们原来也是放行的
（`class C { x = arguments }` 返回 undefined，参考报 SyntaxError）。根因是字段初始值表达式
根本没有自己的解析上下文，直接沿用了外层函数的——`super.x` 在字段里也因同一个原因报
`'super' is only valid in a method`。现在字段初始值有自己的 `FnCtx`
（`in_method: true`、`arguments_allowed: false`），两条都对齐了，
`class B { static m(){…} } class C extends B { static x = super.m() }` 开始工作。

### 18.6 差分先立起来，然后发现 class 从来没被字节码对拍过

按惯例新开 `crates/quickjs-cli/tests/static_blocks.rs`（9 组、整进程输出 + 退出码对拍，含栈帧
里每一列）。这是本轮的主网。

字节码那一层则撞了墙：`disasm_diff.rs` 里**一条 `class` 用例都没有**。写进去才发现问题不在
静态块——同一个类的构造函数和字段初始化器在**父函数常量池里是兄弟**，而参考是**惰性**创建初始化器
（第一个静态字段/静态块出现时才建），我们则在遍历类体之前就把构造函数编完了。于是两者在 dump
里的**先后顺序相反**，整体文本比较必然失败。继续试还暴露出另外几处与静态块无关的分歧：

- 参考的构造函数无条件发 `get_var_ref_check <class_fields_init>; dup; if_false …`，我们只在
  "有实例字段"时才发（`<class_fields_init>` 为 `undefined` 时那串指令只是空转，所以行为一样，
  字节码不一样）；
- 参考按需要决定构造函数用 `push_const` 还是 `fclosure`，我们一律 `fclosure`；
- 收尾的 `close_loc` 选哪个槽、`var_ref_count` 的算法也不同。

（只剩最后这一条还有具体例子：18.2 那个 `push_this` 重复的问题在本轮顺手修掉了——
字段初始化器既由 `begin_this_local` 显式建 `this`，又会被 `provide_this_local` 补一次入口
初始化，于是入口出现两次 `push_this / put_loc0 this`；加一个 `has_this_prologue` 标记后与
参考一致。）

这些都属于"把 class 的编译整体对齐参考"，是独立一轮的工作量，所以**没有**把 class 用例塞进
`disasm_diff`——塞进去只会得到一条长期红的测试，或者一条被削到没有意义的测试。静态块的字节码
本轮是**手工**逐条对过参考 dump 的（18.1、18.4 那两张表就是这么来的）；作为一个局部校验，
把 22 条静态块用例逐个函数体（忽略函数先后顺序）与参考 dump 对比，除上面那两条与静态块
无关的分歧外全部一致。自动化的那一层留作欠账。

### 18.7 验收

- `cargo test` 全绿（新增 `crates/quickjs-cli/tests/static_blocks.rs` 9 项）；
  `cargo fmt` / `cargo clippy -D warnings` 干净
- `scripts/check.sh` 全绿：lang-diff 80/85、realworld 2/2、verify-claims 21/21、NFR-6/7
- test262 `built-ins`：14921/17822 **零变化**（`built-ins` 里本来就没有静态块用例，不动的才是对的）
- test262 `language/statements/class`：1904 → **1946**（+42，-0）
- test262 `language/expressions/class`：1728 → **1749**（+21，-0）
- 把 `test/language/**/class/*static-init*.js`（30 条）单拎出来跑：**29/30**
  （唯一失败是 `static-init-scope-private.js`，卡在私有字段，见 18.8）

### 18.8 发现但未修

- **私有字段**：`static-init-scope-private.js`、以及所有 `#x` 用例，仍未实现。
- **`var` 在静态块里**：spec 规定 `ClassStaticBlockStatementList` 里不许 `var`，参考**不检查**
  （`class C { static { var a = 1 } }` 两边都通过）。本轮跟参考，不跟 spec——这个项目的一致性
  基准是参考实现。
- **strict 模式下删除不可配置属性不报错**：`function f(){ "use strict"; delete Object.prototype }`
  参考抛 `TypeError: could not delete property`，我们静默通过。与静态块无关，写测试时撞上的。
- **顶层直接 eval 里的 `new.target`**：`eval("new.target")` 在脚本顶层，参考报
  `new.target only allowed within functions`，我们返回 `undefined`（参考把
  `new_target_allowed` 沿直接 eval 传下去，我们没有）。
- **参考的 `yield` 措辞**：strict 下参考说 `unexpected 'yield' keyword`，我们说
  `invalid use of strict mode reserved word 'yield'`。位置一致，措辞不一致。
- **class 的字节码还没有自动对拍**（见 18.6）。
- 16.8 的其余项仍在：`new BigInt(1)` 少一个 native 帧、`using` disposer 帧位置、
  那条不稳定的 TypedArray 用例。
