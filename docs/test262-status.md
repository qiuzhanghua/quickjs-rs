# test262 现状（基线测量）

测量时间：2026-11-22（基线提交 `4a30079`，最后一轮修复到 `93ecc32`，均在 v0.4.0 之后）

**可复现**：逐用例结果已入库为 `docs/test262-baseline.txt.gz`（23885 行，
`PASS`/`FAIL`/`SKIP` + 用例名 + 跳过原因）。任何一次新跑都能和它做集合差分：

```sh
TEST262_OUTCOMES=/tmp/now.txt ./scripts/test262.sh
python3 ./scripts/test262-diff.py /tmp/now.txt     # +N fixed / -M regressed
```

差分按**用例名**求，不按总数。这一点不是洁癖：下面第 7 节里"修好 978 例、回归 0 例"
这类结论，全部靠这个比较得出；而总数相同的两次运行，完全可能是一次回归加一次修好
互相抵消。`test262-diff.py` 遇到任何回归就返回非零。

**已修**：4.1、4.2、4.3、4.4、4.7、4.8、4.10，可运行通过率 **42.7% → 61.3%**，
净修好 2739 例、新引入失败 0 例。见第 7 节。

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
| **可运行中通过** | **10845（61.4%）** ← 基线 7536（42.7%） |
| 可运行中失败 | 6820 |
| ↳ 其中挂起（引擎不终止） | 6 |
| ↳ 其中答错 | 6814 |
| 跳过（引擎完全没这功能） | 6220 |

与上一次记录（10836）差的 9 例来自一处代理修复，见 4.11；挂起数从 45 降到 6，是因为
每个测试进程现在都带 `ulimit -v` 512 MiB（见第 8 节），此前"挂起"里混着若干实际是
被内存拖死的用例。

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

### 4.2 原始值接收者未装箱（ToObject）+ 数组方法快照接收者 — ✅ 已修

```js
Array.prototype.map.call(false, function(v, i, o){ return o instanceof Boolean })
// 应为 true（接收者被装箱成 Boolean 对象），实为 false
Object.assign(1, {a: 1})   // TypeError: Object.assign target is not an object，应成功
```

`Array.prototype.map`／`Object.assign` 等泛型方法必须先 `ToObject(this)`，不能把原始值直接当对象或直接拒绝。

修的过程中查出同一个病根下的三件更大的事：

1. **`ToObject` 有两份实现。** `interp::object_from` 是对的，`builtins::this_object`
   是个只写 `_value` 的残缺副本 —— 装箱后的字符串因此没有 `length` 也没有索引属性，
   `map.call("ab", f)` 读不到任何元素。现在只有一份。
2. **数组方法把接收者复制成快照。** 回调的第三个参数拿到的是副本（`o === 原对象` 为
   false），而且回调里删掉的元素照样被访问 —— 规范要求每次读 `Get(O, k)`。对真数组
   也一样错，6 条探针里 5 条答错。
3. **`map` 丢掉空洞。** `[1,,3].map(f)` 返回 2 个元素，应为 `[f(1), <hole>, f(3)]`；
   连带 `includes` 漏掉空洞（应以 `undefined` 命中）、`find`/`findIndex` 不访问空洞。

修法：`receiver` 不再复制；新增 `receiver_element`（逐次 `[[Get]]`，`None` 与
`Some(undefined)` 区分空洞与真值）；8 个改写方法保留旧的复制路径并**明确记为已知缺口**
（见 4.9），因为它们有第二个更大的问题。

`a.length` 上限（4.4）随之修好：`ArrayCreate` 的 `2^32-1` 校验补上，44 条挂起消失。

### 4.3 TypedArray 只是骨架（1365/1453 条）— ✅ 已修（仍失败 643）

```js
var t = new Uint8Array([1,2,3]);
[...t]                  // TypeError: [object Object] is not iterable —— 没有迭代器
t.at                    // undefined
t.map, t.filter         // undefined
typeof BigInt64Array    // undefined
typeof DataView         // undefined
```

**实际缺的比"骨架"更多**，按修的顺序：

1. `BigInt64Array`／`BigUint64Array` 整个不存在（490 条只是 `ReferenceError`）。
   元素读写要按 BigInt 语义：读回 `0n` 且 `typeof` 是 `"bigint"`，写 Number 是
   `TypeError`，写入值按 `2^64` 取模环绕（`BigUint64Array(1)[0] = -1n` 得 `2^64-1n`）。
   这逼出两处架构变动：`ElementType::write` 变成可失败，`read` 需要堆来分配 BigInt
   （从缓冲区自己的 `Gc` 取 `heap()`，不必给每个调用者加参数）。
2. 迭代族（`map`/`filter`/`forEach`/`some`/`every`/`reduce`/`find*`/`indexOf`/
   `lastIndexOf`/`includes`/`at`/`reverse`/`sort`/`copyWithin`/`with`/`toReversed`/
   `toSorted`/`values`/`keys`/`entries`）一个都没有。注意 `map`/`filter`/`slice` 要返回
   **同类型视图**而不是数组。
3. `%TypedArray%` 这个内建本身不存在——`Object.getPrototypeOf(Int8Array)` 是
   `Function.prototype`，所以 `from`/`of`/`Symbol.species` 无处安放。
4. 构造函数只走 array-like 路径，不消费 `Symbol.iterator`；而原始值应该走 `ToIndex`
   而不是迭代（`new Int32Array("3")` 是三个 0，不是三个字符）。
5. `TypedArraySpeciesCreate` 缺失：`map`/`filter`/`slice`/`with`/`toReversed` 必须读
   `constructor[Symbol.species]`，否则子类重定向和抛错都无法观察。
6. 分离缓冲区（detached buffer）的视图没在方法入口 `ValidateTypedArray`，于是静默
   当成空视图而不是抛 `TypeError`。

`length`/`byteLength`/`byteOffset`/`set`/`subarray`/`slice`/`join` 有，其余大面积缺失。这直接推翻了"FR-3 的 TypedArray 部分已完成"的说法。

### 4.4 无长度上限校验导致挂起（44 条）— ✅ 已修

```js
Array.prototype.map.call(new Proxy([], {get: (_, k) => k === 'length' ? 2**32 : undefined}), f)
// 应抛 RangeError（ArrayCreate 的 2^32-1 上限），实则循环 2^32 次
```

这也是 runner 必须每用例一个进程的原因：进程内跑，第一个这类用例就吞掉整轮测量。

### 4.5 正则文法不完整（约 130 条）

`RegExp.escape` 不存在；lookbehind、命名捕获组、`(?i:...)` modifiers 报 `invalid group`。

### 4.8 `Object.defineProperties` 根本不存在 — ✅ 已修（连带修出 5 组规范规则）

它没有被安装，所以一切"预期抛 TypeError"的用例都**因为报错而通过** ——
"undefined is not a function" 恰好也是 TypeError。把方法补上之后，126 条用例从
假通过变成真失败，其中 121 条暴露的是真正的语义缺失：

- 描述符省略的属性应**继承**而非默认 `false`（`{value: 5}` 不该把属性锁死）；
- 非可配置属性不可重定义，但**同值重定义不算变更**（`SameValue`，所以 `NaN` 可以）；
- 数据属性描述符不可同时带 `get`/`set`；
- 不可扩展对象不可新增属性；
- 数组 `length` 是异类全局属性：不可配置、不可枚举、不可变访问器、可单独关闭可写性，
  关闭后不可再打开、不可新增索引，截断时遇到不可删除的元素要失败。

### 4.9 数组下标有两个真相源（已知缺口，未修）

`Object.defineProperty(a, "0", {get})` 现在能生效了（访问器进属性表、元素槽置空洞），
但架构上是"元素向量 + 属性表"两套存储，凡是要修改**接收者本身**的数组方法
（`push`/`pop`/`shift`/`unshift`/`fill`/`splice`/`reverse`/`sort`）仍在写那份快照副本：

```js
Array.prototype.push.call(1, 2)                          // 应返回 1，现抛 TypeError
var o = {length: 2, 0: "a", 1: "b"}; splice.call(o,0,1)  // o 应变成 {0:"b",length:1}
```

读路径（`map`/`filter`/`forEach`/`some`/`every`/`reduce`/`find`/`indexOf`/`includes`/
`join`/`slice`/`concat`/`flat`/`flatMap`/`at`）已按规范逐次 `[[Get]]`，不受影响。

真正的修法是让 `[[Get]]`/`[[Set]]` 都只认属性表、元素向量退化为缓存，或让 8 个改写
方法走 `set_property` 而不是 `array_set`。两种都是跨模块改动，单独排期。

### 4.10 `DataView` 完全不存在（0/520）— ✅ 已修（423/520）

`typeof DataView === "undefined"`，520 条全部以 `ReferenceError` 失败。它与 TypedArray
共享 `ArrayBuffer`，但方法集独立：宽度与字节偏移**每次访问时才指定**，所以对齐无关，
端序也是逐次决定。

复用了 `ElementType::read`/`write`（它们本来就按字节偏移工作），22 个访问器用宏生成——
`NativeFn` 是裸函数指针，宽度只能烘进函数里而不能捕获。

修的顺序里有三件事超出"补一个类"：

1. **`Reflect.construct` 把第三个参数丢了。** `new.target` 从不生效，
   `Object.getPrototypeOf(Reflect.construct(C, [], D))` 返回 `C.prototype`。这是预存在
   的引擎 bug，被 DataView 的 `custom-proto-*` 用例逮到。
2. **修好它之后又暴露一层：原生构造器先建实例、后校验参数。** 规范要求先校验
   （`new Promise(非法executor)` 要在读 `new.target.prototype` 之前抛 TypeError）。
   让所有原生构造器自建实例代价太大（date/number/string 都用"`this` 是对象"判断 `new`），
   所以只在真正有校验的三个上加 `Callable::Native { own_instance }` 标志：
   `Promise`、typed array 各构造器、`DataView`。**这一步第一次尝试是全改，数字掉到 50.4%**，
   靠全量对比才发现代价，改成按需。
3. **`install_tag` 忽略了自己的 `tag` 参数**，永远返回 `"ArrayBuffer"`，
   于是 `Object.prototype.toString.call(dataView)` 报错类型。顺带修掉。
   同时发现 `Symbol.toStringTag` 被装成了访问器，规范是数据属性。

## 5. 对里程碑结论的修正

- FR-3 中 `TypedArray`／`DataView`／`BigInt64Array` 不能记为完成（4.3）。
- FR-3／FR-4 中"Promise 未实现"已单独记录：`typeof Promise === "undefined"`，async/await 运行期直接抛 `InternalError`。251 条 Promise 失败不是"实现有 bug"，是**根本没实现**，只是 `Promise` 目录的用例大多不经过 async 语法才被判成 runnable。
- v0.4.0 文档里"完成"的措辞已按上述收紧。

## 6. 下一步的排序依据

优先级按"修一条能清掉多少条失败"排，而不是按里程碑顺序：

1. ~~`name`/`length` 可配置（4.1）~~——已修，实测 +561 例。
2. ~~`ToObject` 装箱与数组接收者（4.2）~~——已修，连同 4.4、4.7、4.8 共 +704 例。
3. ~~`TypedArray` 补全（4.3）~~——已修，实测 +978 例。
4. ~~`DataView`（4.10）~~——已修，实测 +496 例（0→423）。
4. `Iterator` helpers（664/684 条）——`Iterator` 全局对象整个没有。
5. `Promise`（真做）——229 条 Promise，另加 `flag:async` 632 条跳过的前置条件。
6. 正则文法（4.5）——1092 条，受 Unicode 表依赖，和 FR-8 一起做。
7. 数组下标双真相源（4.9）——8 个改写方法的 array-like 语义。

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

### 4.2 + 4.4 + 4.7 + 4.8：数组接收者、长度上限、定义属性

| | 修前 | 修后 |
|---|---|---|
| 可运行通过 | 8097（45.8%） | **9362（53.0%）** |
| 可运行失败 | 9568 | 8303 |
| 新引入失败 | — | **0** |

逐用例对比：修好 1265 例，回归 0 例。改进分布：`TypedArray`（接收者现在能读到元素）、
`Array` 的 `map`/`filter`/`forEach`/`some`/`every`/`reduce`/`indexOf` 全族、
`Object.defineProperty`/`defineProperties`、`String`（装箱后的字符串有了 `length` 与索引）。

过程中有三次"数字变好但其实是变差"，都在同一轮里查了出来：

1. **把 126 条假通过变成了真失败。** 补上 `Object.defineProperties` 后，原先靠
   "undefined is not a function 也是 TypeError"混过去的用例开始真正失败。
2. **`receiver_element` 每次克隆整个元素向量**，把 100 万长度的稀疏数组变成 O(n²)，
   6 条用例从通过变成挂起。改成原地读。
3. **`isConcatSpreadable` 显式 `undefined` 被当成 false**，导致普通数组不再展开。

三处都不是"顺带发现"，而是每次全量对比（按用例名求差集）报出来的。

回归保护：`crates/quickjs-pure/tests/builtins.rs` 的
`array_methods_are_generic_over_array_likes_and_read_the_receiver_lazily` 与
`define_property_follows_validate_and_apply`。已验证两者在修复前都失败。

### 4.3 TypedArray：从骨架到可用

| | 修前 | 修后 |
|---|---|---|
| `TypedArray` | 84/1446 | **800/1446** |
| `TypedArrayConstructors` | 291/666 | **448/666** |
| 全量可运行通过 | 9362（53.0%） | **10340（58.5%）** |

修好 978 例，回归 0 例（逐用例名字求差集）。

过程中的两次自我修正，都是全量对比报出来的：

1. **`Array.from({length: 2})` 返回空数组。** 这是预存在的 bug（改动前后都是空），
   但它是 typed-array 用例构造源数据的必经之路：`makeArray` 用它建数组，于是所有
   `new TA(Array.from({length: n}, …))` 拿到的视图长度都是 0。根因是缺失的索引被
   *跳过* 而不是填 `undefined`。不修它，TypedArray 的修法看起来对、数字上不去。
2. **`ToNumber` 对 BigInt 静默返回 `NaN`。** `new Int32Array(1n)` 因此得到长度 0 而不是
   `TypeError`。修在 `Interp::to_js_number`（12 处调用点全是规范要求抛错的地方）。
   `Math.max(1n)` 仍答 `NaN`——`Math` 走的是自己的 `to_number()`，记为已知缺口。

回归保护：`crates/quickjs-pure/tests/builtins.rs` 的
`typed_arrays_are_complete_enough_to_iterate_and_transform` 与
`array_from_and_array_index_search_honour_their_arguments`，已验证在修复前都失败。

### 仍未做（同一批里明确留下的）

- **`Iterator` helpers**：664/684 失败，`Iterator` 全局对象整个没有。
- **`Math` 缺 `ToNumber` 的 BigInt 拒绝**：`Math.max(1n)` 应抛 `TypeError`。
- **`DataView`**：当时 0/520 完全缺失，随后在同一轮里修掉，见 4.10。

### 4.10 DataView

| | 修前 | 修后 |
|---|---|---|
| `DataView` | 0/520 | **423/520** |
| 全量可运行通过 | 10340（58.5%） | **10836（61.3%）** |

修好 496 例，回归 0 例。

最值得记的一次误判：为了让"先校验参数、再读 `new.target.prototype`"成立，我一开始把
**所有**原生构造器都改成自建实例（`this = undefined`），整体从 61% 掉到 **50.4%**。
原因是 date/number/string 等构造器一直用"`this` 是不是对象"来区分 `new C()` 与 `C()`，
`this` 变成 `undefined` 后它们把两者都当普通调用。全量对比立刻报出 722 条回归，
才把改动收窄成"只有带参数校验的三个构造器自建实例"。

如果只看那两个 `custom-proto` 用例是否变绿，这个代价完全看不见。

### 仍未做（同一批里明确留下的）

- **`Iterator` helpers**：664/684 失败，`Iterator` 全局对象整个没有。
- **`RegExp`**：1091 失败，受 Unicode 表依赖，和 FR-8 绑在一起。
- **`Math` 缺 `ToNumber` 的 BigInt 拒绝**：`Math.max(1n)` 应抛 `TypeError`。
- **`DataView` 的 resizable ArrayBuffer**：97 条剩余里 22 条是它，需要
  `ArrayBuffer.prototype.resize` 整套。

## 8. 让全量跑变得安全（与 4.11 同批）

接上 oracle 之后第一次跑全量，**把用户的会话跑掉了**：机器被登出。原因不是测试崩了，
是测试把整机内存吃光。

### 三个叠加的原因

1. **并发数跟着核数走。** runner 原本用 `available_parallelism()`，
   在 28 核机器上就是 28 个并发引擎，每个跑一个 test262 用例。
2. **单进程没有内存上限。** `qjs --memory-limit 64M` **是装饰**：
   `Runtime::set_memory_limit` 只把值存进字段，引擎里没有任何一处读它。
   实测 `qjs --memory-limit 64M -e 'var a=[];for(;;)a.push(new Array(1e5))'`
   一直吃到外部 `ulimit` 的 2 GB 才 abort，而不是在 64 MB 处停下。
   `--stack-size` 同理。这两个都是 CLI 接受、帮助文本宣传、实际空转的选项。
3. **内存耗尽时是 panic 而不是报错。** 分配失败直接
   `memory allocation of N bytes failed` + 核心转储，`Error::OutOfMemory` 那条路径
   从未被执行到。

三者相乘：一个故意申请大数组的用例，在一个不设限的进程里，复制 28 份。

### 修法（先止血，不是根治）

- runner 改为用 `sh -c 'ulimit -v 512MiB; exec "$0" "$@"'` 起引擎，
  上限加在**子进程**上，所以即使引擎自己的限额是装饰也照样成立。
- 默认并发封顶 `min(核数, 8)`，可用 `TEST262_JOBS` 覆盖。

改完后同一个"无限申请"用例变成一个普通 `FAIL`（`memory allocation ... failed`），
机器内存全程稳定，全量跑 3 分半结束。

### 仍然是缺口的

- **`--memory-limit` / `--stack-size` 没有实现。** 这是**响亮失败原则的空白**：
  选项被接受、被写进 `--help`、被 `Runtime` 存下，然后什么也不做。
  按本项目的尺子，它应该抛错而不是静默忽略。
- **分配失败应走 `Error::OutOfMemory`**，而不是让 Rust 的分配器 abort。
  没有 `set_error_hook` 式的兜底，任何 OOM 都会变成核心转储。这需要把
  `Vec` 的分配点收进一个可控的分配器，属于跨模块改动。

### 4.11 代理的可调用性在撤销后被丢掉（+9 例）

接线之后 `lang-diff.py` 从 SKIPPED 变成 `79/85`，唯一那条 gap 是
`test_builtin.js test_function`，暴露出三处同源的静默错答案：

```js
var p = Proxy.revocable(function f(){}, {}); p.revoke();
typeof p.proxy                              // 应为 "function"，实为 "object"
Function.prototype.toString.call({})        // 应为 TypeError，实为返回 "[native code]"
Function.prototype.toString.call(p.proxy)   // 应为字符串，见上
```

根因是 `is_callable()` 对代理的判据写成了"目标**现在**还在不在"
（`target.as_ref().is_some_and(...)`）。撤销把 `target` 置空，于是撤销后的代理
连带丢掉了 `[[Call]]`——而规范里撤销只影响 trap 转发，不影响代理本身是否可调用。

修法是**创建时记下**可调用性（`ProxyState::callable`），而不是每次从 `target`
重新推导。顺带补上 `Function.prototype.toString` 对非可调用接收者的 `TypeError`
——此前它对任何对象都返回 `[native code]`，这恰好也是"报错被当成通过"的那一类：
`assert_throws(TypeError, ...)` 在实现完全不校验时也能过。

实测 `typeof revoked.proxy === "function"` 与三处 `TypeError` 均与参考一致，
全量 +9 例（10836 → 10845），回归 0 例。
