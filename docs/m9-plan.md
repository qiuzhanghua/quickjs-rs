# M9 拆解：`std`/`os` 宿主模块 + 事件循环 + CLI/REPL

状态：**M9a/M9b 已完成/M9c 已完成**（提交见 git log），M9b 剩余与 M9d 待开工
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

`--memory-limit`/`--stack-size` 建议单独列成 M9e 或并入 M11，它们和"能嵌入使用"
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
