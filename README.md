# quickjs-rs

用**纯 Rust** 重写 [bellard/quickjs](https://github.com/bellard/quickjs)（基线 `VERSION = 2026-06-04`），
目标：不依赖 C 代码即可构建，功能/语义等价，可替换现有 quickjs 使用场景（嵌入式脚本、CLI 解释器、AOT 编译）。

- 📄 需求分析：[`docs/requirements.md`](docs/requirements.md)（v0.2，关键决策已确认）
- 🚧 当前阶段：M0 之前（骨架未落地）

## 已确认决策

| 决策点 | 结论 |
|---|---|
| API 形态 | 惯用 Rust API 为主；C ABI 兼容层做独立 crate（M12） |
| 功能范围 | 全量：核心 ES2025 + Temporal + Intl（ECMA-402/CLDR，独立 feature） |
| 字节码 | 自有稳定格式，不与 C 版二进制互通；保留 qjsc AOT |
| GC | `Rc` + 循环回收（引用计数 + trial-deletion），确定性释放 |

## 一致性基线

以 C 版 `qjs` 实测输出为 oracle（本机可构建）：

```sh
curl -o qjs.tar.gz https://codeload.github.com/bellard/quickjs/tar.gz/refs/heads/master && tar xzf qjs.tar.gz
cd quickjs-master && make -j8 qjs qjsc
./qjs tests/microbench.js     # total ≈ 4254
```
