# quickjs-rs

用**纯 Rust** 重写 [bellard/quickjs](https://github.com/bellard/quickjs)（基线 `VERSION = 2026-06-04`），
目标：不依赖 C 代码即可构建，功能/语义等价，可替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译）。

- 📄 需求分析：[`docs/requirements.md`](docs/requirements.md)（v0.2，关键决策已确认）
- 🚧 当前进度：**M1a 完成**（词法分析器），M0（骨架 + `js-gc` + 差分框架）已完成

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
| M1b | AST + 解析器 + early errors | ⬜ |
| M2 | 字节码编译器 + 作用域解析 + 序列化 | ⬜ |
| M3 | VM 核心（调用/对象/异常/闭包/原型） | ⬜ |
| M4–M11 | 内建库、ES6+ 运行期、模块、正则/Unicode、BigInt/TypedArray、std/os/Worker、AOT、性能 | ⬜ |
| M12–M14 | C ABI 层、Temporal、Intl | ⬜ |

## M0 已交付

```
crates/js-gc/                引用计数 + 循环回收（16 个测试）
crates/js-parse/             词法分析器（33 个单元测试 + 5 个差分测试）
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

## 提交约定

每个里程碑（M0、M1、…）单独提交，提交前必须 `./scripts/check.sh` 全绿；
提交信息用 `feat(module): 中文摘要 (Mx)`，正文写清验收证据（测试数、与 C 版对比结论）。
