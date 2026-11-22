# test262 现状（基线测量）

测量时间：2026-02（提交 `4a30079`，v0.4.0 之后）

**已修**：4.1（`name`/`length`/`prototype` 属性）与 4.7（匿名类表达式）已修复，
可运行通过率 **42.7% → 45.8%**，净修好 561 例、新引入失败 0 例。见第 7 节。

复现：

```sh
./scripts/test262.sh                      # 全部 test/built-ins
TEST262_GROUP=1 ./scripts/test262.sh      # 按部件分组
TEST262_SIGNATURES=1 ./scripts/test262.sh # 失败形态聚类
```

---

## 1. 为什么要先测这个

项目原有的差分套件（`scripts/lang-diff.py`）跑的是上游自己的 `tests/*.js`——七个文件，切片 runner 实际只看五个。它是个合格的回归网，却是个不合格的覆盖率度量：整个语料里 TypedArray 只出现约五次，私有类字段一次都没有。于是"80/85 通过、无已知缺口"这句话在 `[...new Uint8Array([1])]` 抛异常、`#x` 编译不了的时候依然成立。

**窄样本上的绿色数字，对被样本未触及的部分不构成任何证据。**

test262 是标准自己的套件，`requirements.md` 本来就把"test262 对应用例通过"写成了 FR-3／FR-5 的验收口径。所以无论如何都要接；先测的意义在于，让之后"我们修好了"这句话可被检验。

## 2. 数字

| | 数量 |
|---|---|
| `test/built-ins` 用例总数 | 23885 |
| 可运行（runner 不跳过） | 17665 |
| **可运行中通过** | **7536（42.7%）** |
| 可运行中失败 | 9568 |
| ↳ 其中挂起（引擎不终止） | 44 |
| ↳ 其中答错 | 9524 |
| 跳过（引擎完全没这功能） | 6220 |

跳过不是"通过"，所以它单列。按原因：

| 原因 | 数量 |
|---|---|
| `feature:Temporal` | 4624 |
| `flag:async`（`$DONE` 未接线） | 632 |
| `feature:Atomics` | 242 |
| `feature:SharedArrayBuffer` | 233 |
| `feature:explicit-resource-management` | 202 |
| `runner: $262.createRealm` 未实现 | 167 |
| `runner: $262.agent` 未实现 | 112 |
| `runner: $262` 在模块用例中不可用 | 8 |

`runner:` 那 287 条是**本 runner 的缺口，不是引擎的**。Temporal（4624）占了跳过的 74%，它不在 `requirements.md` 的范围内，这只是把"没打算做"与"做了但坏了"分开。

## 3. 失败面在哪

按部件（仅可运行）：

| 部件 | 失败数 |
|---|---|
| Object | 1567 |
| Array | 1383 |
| TypedArray | 1365 |
| RegExp | 1115 |
| Iterator | 664 |
| String | 568 |
| DataView | 520 |
| TypedArrayConstructors | 472 |
| Promise | 251 |
| Set | 213 |

按失败形态（同一缺陷会聚成一类）：

| 数量 | 形态 | 等价于 |
|---|---|---|
| 1086 | `TypeError: undefined is not a function` | 缺失方法（`RegExp.escape`、`TypedArray.prototype.at/map/filter` 等） |
| 806 | `ReferenceError: BigInt64Array is not defined` | BigInt 类型化数组完全没有 |
| 465 | `ReferenceError: DataView is not defined` | `DataView` 完全没有 |
| 441 | `ReferenceError: Iterator is not defined` | Iterator helpers 没有 |
| 435 | 期望抛 `TypeError` 却没抛 | 缺参数/类型校验 |
| 520 | `name`/`length` 描述符应可配置 | **一个系统性缺陷，见 4.1** |
| 192 | `TypeError: target is not an object` | **原始值未装箱，见 4.2** |
| 192 | 期望 parse 期 `SyntaxError` 却执行了 | 早期错误检查缺失 |
| 57 | `ShadowRealm is not defined` | 未实现（非目标） |
| 70+58 | 正则 `invalid group`／lookbehind 不支持 | 正则文法不完整 |

## 4. 已确认的引擎缺陷（有最小复现）

### 4.1 函数的 `name`/`length` 不可配置（约 520 条）— ✅ 已修

```js
Object.getOwnPropertyDescriptor(function f(a){}, "name").configurable  // 应为 true，实为 false
```

两个属性都按 ES 规范应为 `configurable: true`。影响 `Object.assign`、`Function`、每个内建方法的 `length.js`／`name.js` 用例——这类用例每个部件都有几个，所以它是**唯一一条横跨所有部件的缺陷**，修一次收益最广。

根因不是漏写某个属性，而是 `Property::with_attributes(value, writable, enumerable)` 把
`configurable` 绑死在 `writable` 上。这个推导对"内建方法"（`writable:true, configurable:true`）
成立，而对函数上另外两种形态都不成立：

| 属性 | 规范 | 旧推导 |
|---|---|---|
| 内建方法 | `writable:true, enumerable:false, configurable:true` | 对 |
| `name` / `length` | `writable:false, enumerable:false, configurable:true` | 错（两个方向里的第一个） |
| 函数 `prototype` | `writable:{用户函数 true, 内建 false}, enumerable:false, configurable:false` | 错（第二个） |

已新增 `Property::name_or_length` 与 `Property::prototype_of` 把这两个例外收进一处，
并在 `with_attributes` 的文档里写明它只对第一种成立。

连带修掉的三处（同一类错误的其它出口）：
- 匿名函数／箭头函数的 `name` 属性**根本不存在**，应为存在且为 `""`；
- 类构造器的 `length` 被写成 `enumerable: true`（应为 `false`）；
- 用户函数的 `prototype` 被写成 `configurable: true`（应为 `false`）。

### 4.7 匿名类表达式求值为 `undefined` — ✅ 已修

```js
var x = class {}; print(x)          // 实为 undefined
print(typeof (class {}))            // 实为 TypeError: undefined is not a function
```

编译器在类作用域里用 `name.unwrap_or_default()` 去 `SetInit` 一个绑定，匿名类没有这个绑定，
于是对空字符串名字发出一条全局存储指令。修法是只在确实有类名时才初始化该绑定。

修好后 NamedEvaluation 也随之正确：`var x = class {}` 的 `x.name === "x"`，而 `(class {}).name === ""`。

### 4.2 原始值接收者未装箱（ToObject）（约 192 + Array 一批）

```js
Array.prototype.map.call(false, function(v, i, o){ return o instanceof Boolean })
// 应为 true（接收者被装箱成 Boolean 对象），实为 false
Object.assign(1, {a: 1})   // TypeError: Object.assign target is not an object，应成功
```

`Array.prototype.map`／`Object.assign` 等泛型方法必须先 `ToObject(this)`，不能把原始值直接当对象或直接拒绝。

### 4.3 TypedArray 只是骨架（约 1900 条）

```js
var t = new Uint8Array([1,2,3]);
[...t]                  // TypeError: [object Object] is not iterable —— 没有迭代器
t.at                    // undefined
t.map, t.filter         // undefined
typeof BigInt64Array    // undefined
typeof DataView         // undefined
```

`length`/`byteLength`/`byteOffset`/`set`/`subarray`/`slice`/`join` 有，其余大面积缺失。这直接推翻了"FR-3 的 TypedArray 部分已完成"的说法。

### 4.4 无长度上限校验导致挂起（44 条）

```js
Array.prototype.map.call(new Proxy([], {get: (_, k) => k === 'length' ? 2**32 : undefined}), f)
// 应抛 RangeError（ArrayCreate 的 2^32-1 上限），实则循环 2^32 次
```

这也是 runner 必须每用例一个进程的原因：进程内跑，第一个这类用例就吞掉整轮测量。

### 4.5 正则文法不完整（约 130 条）

`RegExp.escape` 不存在；lookbehind、命名捕获组、`(?i:...)` modifiers 报 `invalid group`。

## 5. 对里程碑结论的修正

- FR-3 中 `TypedArray`／`DataView`／`BigInt64Array` 不能记为完成（4.3）。
- FR-3／FR-4 中"Promise 未实现"已单独记录：`typeof Promise === "undefined"`，async/await 运行期直接抛 `InternalError`。251 条 Promise 失败不是"实现有 bug"，是**根本没实现**，只是 `Promise` 目录的用例大多不经过 async 语法才被判成 runnable。
- v0.4.0 文档里"完成"的措辞已按上述收紧。

## 6. 下一步的排序依据

优先级按"修一条能清掉多少条失败"排，而不是按里程碑顺序：

1. ~~`name`/`length` 可配置（4.1）~~——已修，实测 +561 例。
2. `ToObject` 装箱（4.2）——Array/Object 一批，约 300+ 条。
3. `Promise`（真做）——251 条 Promise 加上 `flag:async` 632 条跳过的前置条件。
4. TypedArray 补全（4.3）——约 1900 条，但工作量大，建议在 1–3 之后。
5. 正则文法（4.5）——受 Unicode 表依赖，和 FR-8 一起做。

前两项是小改动大杠杆，也正好是"静默错答案"而非"报错"的那一类，风险高于数字所示。

## 7. 修复记录

### 4.1 + 4.7：函数属性与匿名类表达式

| | 修前 | 修后 |
|---|---|---|
| 可运行通过 | 7536（42.7%） | **8097（45.8%）** |
| 可运行失败 | 10129 | 9568 |
| 新引入失败 | — | **0** |

逐用例对比（前后各跑一次全集，按用例名求差集）：修好 561 例，回归 0 例。改进分布：
`Date/prototype` 85、`Array/prototype` 62、`String/prototype` 55、`RegExp/prototype` 23、
`Map/prototype` 20、`Set/prototype` 16、`Function/prototype` 15、`Object/prototype` 14。

回归保护：`crates/quickjs-pure/tests/builtins.rs` 的
`function_name_length_and_prototype_have_the_spec_attributes`。已验证该测试在修复前
会失败（`left: "false,false,false"`），不是装饰。
