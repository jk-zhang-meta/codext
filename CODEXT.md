# codext

`openai/codex` 的分叉。基线 `rust-v0.150.1`，分支 `codext`，上游远端名 `upstream`。

存在的唯一理由：**让 Codex 的凭据来自一个自建的账号池服务，而不是本机的
`auth.json`**。一个人跑很多项目，用量摊在若干个账号上，谁都不像机器。

## 第一原则：除了这两块，一律与上游保持一致

codext 只有两块自己的东西：

1. **凭据池**——`login/src/auth/pool.rs` 和它的几个挂钩点。
2. **错误提示与重试措辞**——「会话由用户结束，不由错误结束」那一套。

**其余一切都应当与上游逐字节相同。** 这不是洁癖，是这个分叉能长期活下去的唯一
方式：每多一行分歧，就多一处每次合并都要重新解释、重新解冲突、并且可能在某次
上游重构里静默失效的地方。判断一个改动该不该做，先问它属不属于上面两块；不属于
就不要做，哪怕它「顺手」「更好」「只有一行」。

这条原则在实践中最常被下面三种情况试探，答案都是"不"：

- **「改上游一行比在自己模块里绕开省事」**——`is_retryable()` 那次就是。把我们的
  重试策略写进全仓共用的错误分类，省了几行，代价是 0.147.0 新增的 guardian 一上来
  就被绊倒。策略留在 `responses_retry.rs`，分类还给上游。
- **「翻一个默认值就行」**——换模型提示那次。翻 `unwrap_or(false)` 只要两行，但会
  让我们的行为与上游不同，并打破 20 个上游测试（那些测试断言的正是上游默认值）。
  正解是用上游本来就支持的配置项（`~/.codex/config.toml` 的 `[notices]`）表达偏好，
  代码零分歧。
- **「关掉某个 feature 能让它编出来」**——`v8_enable_sandbox` 那次。关掉能把
  `code-mode-host` 的构建从一小时变成十分钟，但那会让我们的 code mode 隔离强度与
  上游不同。正解是 `V8_FROM_SOURCE=1` 从源码编，保留 feature、不动上游文件。

反过来也成立：**补齐上游有而我们缺的东西，是"向上游看齐"，不是加私货。** 把
`codex-code-mode-host` 打进发布包属于这一类——上游本来就分发它，是我们的单文件
打包让 codext 少了这个能力。

## 上游footprint：挂钩点

| 文件 | 改动 | 近 5 版上游改动次数 |
| --- | --- | --- |
| `codex-rs/login/src/auth/mod.rs` | `mod pool;` 和几个 re-export | 1 |
| `codex-rs/login/src/auth/manager.rs` `shared()` | 装 provider | **0** |
| `codex-rs/login/src/auth/manager.rs` `load_auth()` | 池子给不出号时退回本地 | 3 |
| `codex-rs/login/src/auth/manager.rs` 续期分流 | 本地那份走本地续期 | 3 |
| `codex-rs/core/src/session/mod.rs` `record_token_usage_info()` | 一次调用结束时记用量 | — |
| `codex-rs/core/src/responses_retry.rs` `RetryKind::of()` | 撞墙时上报被拒 | — |
| `codex-rs/login/src/auth/pool.rs` | 新文件，全部实现 | — |
| `codex-rs/login/src/auth/pool_tests.rs` | 新文件 | — |

没有新增依赖，没有动 `Cargo.toml`。

`core` 侧的两个挂钩点都是一句静态函数调用，和早先的 `pool_is_exhausted()` 同一个
形状——`login` 不能反向依赖 `core`（会成环），所以走进程级静态而不是注册回调。
挑这两个位置的理由和挑 `shared()` 一样，是它们各自唯一：`record_token_usage_info`
是所有 `ResponseEvent::Completed` 的必经之路，`RetryKind::of` 是所有重试分类的
必经之路。

`responses_retry.rs` 里还有一批更早的换号/重试改动（`RetryKind` 那一整套）没有在
这张表里逐条列出，合并上游时要连整个文件一起核对。

挂钩点是按**函数级** churn 选的，不是文件级：`manager.rs` 在最近 5 个 release
里改了 14 次，但 `shared()` 一次没改过。后两个挂钩点在 `load_auth()` 附近，那里
近 5 版动过 3 次（含一次 "unify external auth resolution" 的重构），合并上游时
优先核对它们。

```
git log -L :shared:codex-rs/login/src/auth/manager.rs --oneline rust-v0.142.0..rust-v0.146.0
```

选文件级最少改动的地方是常见错误——一个高频文件里的低频函数，比一个低频文件里
的高频函数安全得多。

## 上游本来就留好了口子

`codex-rs/login/src/auth/manager.rs`：

```rust
pub trait ExternalAuth: Send + Sync {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth>;
    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth>;
}
```

`AuthManager::load_auth()` **优先**问这个 provider，`AuthManager::auth()` 每次取
凭据都重新 `resolve()` 一遍。于是：

- 完全不碰 `auth.json`：不需要文件监视器、不需要保持 inode、不需要防半截写入
- 不受「`CODEX_HOME` 启动后不可更改」的限制，同一台机器上的多个进程各租各的号
- `CodexAuth::from_external_chatgpt_tokens()` 内部就写 `refresh_token: String::new()`
  ——**上游原生支持不带 refresh token 的凭据**，这正是「在线租借，不下发 RT」需要的

## 调度算法

**决策在服务端，每个请求一次。** `resolve()` 没有任何本地缓存短路，每次取凭据都
向池子问一次该用哪个号。

### 为什么不在客户端缓存

缓存住就意味着手上的号额度跑满、被停用、被冷却之后照样继续用，直到某个计时器
到点。多花一个 HTTP 往返（几十毫秒，相对于一次模型调用可以忽略）换来的是配额
状态即时生效。

### 稳定性是决策函数**形状**的性质

```
decide(held):
    if held is not None and usable(held):
        return held              # 不排序、不比较、不看别的号
    return best_candidate()
```

`usable()` 是对单个号的**绝对判定**，永远不是比较。由此可以直接陈述保证：

> 一个终端换号的次数 == 它手上的号变成不可用的次数。
> 永远不会因为「别的号看起来更宽裕」而换。

这就是 prompt 缓存的保障。实测一次两回合的小会话输入 token 里 62% 是缓存命中，
长会话能到 80% 以上；换一次号丢掉整个前缀缓存，为一点余量去换是净亏。

抖动在结构上不可能发生，不是靠阈值压住的：`status` 只由后台操作改变，`cooldown`
有确定的结束时刻，而 `headroom` 在一个窗内**单调下降**、只在重置时跳回去——所以
它向下穿过阈值每个窗最多一次。

### 两个门槛，不是一个

```
PICK_HEADROOM = 5.5%   新派一个号要求的最低余量（94% 以下才派）
KEEP_HEADROOM = 3.5%   已经在用的号骑到这里才换（97% 就换手）
```

不是为了防抖（上面已经说明不会抖），是**换号代价不对称**：换一次丢掉整个缓存；
而新派一个快见底的号，跑几个请求又得再换，缓存代价付两遍。

**两个数都刻意不落在整数刻度上。** OpenAI 报的 `used_percent` 是整数台阶，而
`1 - used/100` 在 IEEE754 里不精确：99% 算出来是 `0.010000000000000009`，比 0.01
**大**。门槛取 1% 时 99% 被判成"还能用"，于是一路骑到 100%——而读到 100% 的时候，
那一次请求已经被拒了。门槛必须在撞墙**之前**被跨过，取半档就与浮点误差无关了。

这不是假想：2026-08-03 一个号就是这样从 0% 一路跑到 100% 然后把会话打断的。回归
测试见 `test_the_bar_is_crossed_before_the_account_hits_the_wall`。

### 准入和排序用两个不同的余量

**这一条最容易改坏。**

```
准入  headroom(horizon=0)    「这个号现在发得出请求吗」
排序  headroom(horizon=600)  「这个号长远宽不宽裕」
```

一个 5h 窗已用 99.9%、五分钟后重置的号：排序该把它当作不稀缺（它确实马上满血），
但准入必须拦住它（下一个请求是现在发的，现在发就是 429）。两处混用一个值的话，
它的 5h 窗会因为在视野内重置而被整条跳过，算出 0.95 的余量排到第一位，然后每个
请求都撞墙。

### 排序

```
score(a) = headroom(a, horizon=600) / (1 + holders(a))
```

- **取 min 不取平均**：周窗才 5%、5h 窗已经 98% 的号，此刻没有余量
- **并发做除数不做减数**：挂着 n 个会话的号消耗快 n+1 倍，单个会话能指望的期望
  余量就是 `headroom/(n+1)`。余量大三倍的号才扛得住三个会话——并发因此被自然
  摊开，不需要硬上限。这也是几台机器同时失去账号时不会一窝蜂涌向同一个号的
  原因：写租约是事务性的，第一台占上之后那个号的分数立刻减半
- **窗口是否过期用 `resets_at` 判**，不用「读数多旧算旧」的阈值

### 手上是哪个号，由客户端带上来

不从服务端的租约表里读。服务端重启、租约行被清理、库回滚，粘性都不受影响；租约
表只用来数并发和做仲裁。

### 额度读数搭同一趟往返上来

用量和 `rate_limits` 跟着派号请求一起发，服务端**先记读数、再判断、最后派号**。
分成两个接口的话，调度总是拿着上一次上报时的旧数据在做这一次的决定。

读数从 `CODEX_HOME/sessions` 下的 rollout 里扫（`token_count` 事件的
`rate_limits`），节流到 5 秒一次。两个易错点：

- 累计量取**最后一条** `token_count` 事件而不是求和——`total_token_usage` 本身
  就是累计值，相加等于把消耗乘以回合数
- 全 null 的窗口（OpenAI 在两次刷新之间会发）不能当成「用量为零」报上去

### 会话结束后的那一轮

一次 `codex exec` 的最后一次响应写完 rollout 就退出了，没有下一个请求会把它带
上去。所以有个 20 秒的空闲心跳，走的是同一个 `current()`，不是第二套上报逻辑。

### 池子给不出号时退回本机 auth.json

**池子优先，永远先问池子；问不出来才退回本地。** 这是每次取凭据的降级，不是切换：
provider 一直装着，下一次照样先问池子，号一回来立刻切回去。

区分两种"给不出"，处置不一样：

| 情况 | 处置 |
| --- | --- |
| 连不上，手上还有租约 | 继续用手上那份（几十毫秒的抖动不该打断会话） |
| 连不上，手上没有租约 | 退回本机 `auth.json` |
| 服务端明确回 `data: null` | **丢掉手上那份**，退回本机 `auth.json` |

第三行是关键。`data: null` 说明服务端不打算再发手上这个号了——它额度到顶、被停用
或在冷却。继续骑着只会一路 429，而 429 不触发 `refresh()`，永远逃不出来。所以这条
路上不复用旧租约，并且把它从内存里删掉：留着的话下一个请求还会把它当作"我手上的
号"报上去，连带把退回本地之后跑掉的用量和额度读数记到它头上，污染调度用的读数。

两个实现细节，不写下来下次会重新踩：

- 池子发的凭据被 `commit_external_auth` 镜像进了进程内的 **Ephemeral 存储**，而
  上游的本地加载**优先读它**。退回本地时必须先把它删掉，否则拿回的还是刚刚用不了
  的那份，等于没退。
- 续期在 provider 模式下采用**有界交替**：先尝试池子；若当前缓存的是本机
  `CodexAuth::Chatgpt` 且池子失败，再尝试本地 refresh token。下一次调用仍从池子
  开始，因此不会因一次失败永久困在本地，也不会在一次调用里递归热循环。provider
  发的 `Headers` 等凭据仍只走 provider 路径，别收窄成
  `is_external_chatgpt_tokens()`，那会打断上游的 bearer 路径。

主动续期没做：`auth()` 在 provider 模式下跳过 `should_refresh_proactively`，所以
退回本地之后那份凭据要等一次 401 才被动续期。多一个往返，够用了。

### 配额耗尽：先靠读数提前换，撞上了再换号重试

**正常路径不需要终端报告。** 读数每个请求都在更新，`headroom` 掉到门槛以下调度
自己就换号了；窗口重置时 `resets_at` 一过，那个窗被跳过，账号自动回到轮换里。

但读数是**上一次调用**留下的，天然滞后一到两档，而多台机器共用一个号时还会被
别人的消耗甩开。所以真撞上 429 时还有第二道：`UsageLimitReached` 不再终结这一轮，
而是落进重试循环——重试会重新取一次凭据，池子据此换一个还有余量的号；没有池子时
就按自己那个窗口重置的节奏慢慢等。这条路上有三个必须知道的约束：

- **重试至少等 6 秒**（`ACCOUNT_SWAP_MIN_DELAY`）。⚠️ **这个数原来的理由已经过期**：
  它写的是"扫 rollout 的读数节流 5 秒"，而 `USAGE_SCAN_MIN_INTERVAL` 早就不存在了
  （扫 rollout 那套换成了 `record_turn_usage` 的记账点）。现在换号靠的是
  `report_account_refused()` 即时上报 `reject=usage_limit`，不需要等扫描。6 秒仍然
  留着是因为它给服务端留了一趟往返的余量，但**别再按旧理由去推导它**。
- **换号时必须丢掉 websocket 连接**（`client.rs`）。上游只有"401 后续期"这一种换
  凭据的情形，账号身份不变，所以连接一直是复用的；租借模式下账号会变，而连接还开
  着就不会用新凭据重连——不显式重置的话，换号在 websocket 传输上完全不生效。连带
  要丢掉 `previous_response_id` 和 turn state 头，它们都是按账号绑定的。
- **池子一个号都发不出来时**（`pool.rs` 的 `POOL_EXHAUSTED` / `EXHAUSTION_ANNOUNCED`）
  报一次"去后台加号"，然后每 30 秒回去问一次。通报是边沿触发的：多个会话同时撞上
  只报一条，恢复供号时标记清掉，下一次枯竭照样报。

**账号级这一族有四个，不是一个**（2026-08-09 扩的）：`UsageLimitReached`、
`QuotaExceeded`、`UsageNotIncluded`、以及状态码是 429 的 `RetryLimit`。

后三个原来都不算，理由写的是"计费状态换号救不了它"——**那是单账号时代的判断**。
一个号的账单确实不会因为等待而恢复，但池子里另一个号有它自己的额度、账单和套餐。
判据是 `held_account_email().is_some()`，即"池子**此刻**在不在供号"：退回本机
`auth.json` 之后手上是用户自己的号，那时确实换无可换，仍归 `Stuck`。

`UsageNotIncluded` 并进来还有一条硬证据：**上游自己**把这三个一起映射成
`CodexErrorInfo::UsageLimitExceeded`。429 那个要用状态码判，因为传输层自己重试耗尽
也复用 `RetryLimit`，但它伪造的状态码是 500。

不算账号级的代价不只是"少换一次号"：`RetryKind::of` 里**只有账号级分支**会
`report_account_refused()`，不报的话服务端要等后台观测（最快 30 秒）才知道这个号
废了，这期间它会一次次把同一个跑满的号发回来——重试在做无用功。

冷却只剩一个用途：终端报 401（上游 `ExternalAuthRefreshReason` 只有 `Unauthorized`
一个变体），服务端把这个号按下去 180 秒，够它的续期任务跑一轮。

### 会话由用户结束，不由错误结束

`responses_retry.rs`。上游在重试循环里问的是"再试一次有没有希望"，答案是否就结束
这一轮。codext 问的是另一个问题：**这一轮该不该结束**。断开一个跑到一半的会话，
代价永远高于多等一会儿——能修的错误（模型名写错、欠费、代理配错）用户看到提示以后
可以去修，修好了下一次重试就通了；修不了的，用户按 Esc。两条路都比替他做决定强。

所以采样路径上**没有次数上限，而且只剩一个出口**（2026-08-09 收敛到这里）：

| 出口 | 为什么它不能重试 |
|---|---|
| `TurnAborted` / `Interrupted` | 这**就是**用户按下的暂停。重试它们等于让 Esc 失灵，而"随时可以自己叫停"正是无限重试能成立的前提 |

原来还有三个，都拿掉了。**三条论据当初听起来都成立，实际都不成立**，而它们错判的
代价是掐断一轮跑到一半的会话：

| 曾经的出口 | 原论据 | 为什么不成立 | 现在 |
|---|---|---|---|
| `CyberPolicy` | "安全判定是答复不是故障，重试问不出别的结果" | 前提是判定确定性成立。实际这个分类器**误判很常见**，同样内容重发经常就过 | `RetryKind::CyberFlag`，无限重试，措辞同时给出"可能是误判"和"改写请求也能解" |
| `ContextWindowExceeded` | "重试一万次还是装不下" | 前提是**中间不压缩**。压缩恰恰就是把它变短的手段，只是上游把压缩挂在 token 阈值的预判上，阈值没拦住的那次就没人管 | `turn.rs` 里当场中途压缩一次再继续；只压一次，压完还满说明单条内容超窗，那时结束才诚实 |
| `SessionBudgetExceeded` | "用户自己设的上限，绕过等于把设置作废" | **这条其实还成立**——拿掉它确实让那个配置失效。是按"除了 Esc 都别停"的口径有意为之 | 恢复方式写在 `ends_the_turn` 的注释里，取消一行注释即可 |

其余一切都无限重试。分类只决定**等多久、怎么说**（`RetryKind`），不决定要不要重试：
池子枯竭 30 秒一次、换号 6–60 秒、没池子等窗口重置 60–600 秒、其余走上游退避曲线
封顶 60 秒。每一类**只报一次**——每隔几十秒刷同一句话等于把提示自己淹掉，而一个
不断增长的 attempt 计数看起来像是坏了，不像在等。

三个容易踩的地方：

- **`stream_max_retries` 现在是退出开关。** 没设过 = 永不放弃；设过 = 一字不差回到
  上游（退避曲线、日志、"Reconnecting... n/m" 措辞全都是）。这样"我不想要这个行为"
  是一句配置的事。但**账号级失败、池子枯竭、`ServerOverloaded` 这三项不受这个开关
  约束**（`responses_retry.rs` 的 `retry_is_allowed`）——它们是这套东西存在的理由，
  不该被一个配置项关掉。

  代价是上游 `core/tests/suite/retry_after.rs` 里那批"过载/限流最终会终止"的用例
  （`*_overload_*_is_terminal`、`*_rate_limit_*_is_terminal`、
  `responses_http_overload_without_retry_after_exhausts_request_retries` 等）
  **按设计永远不会绿**：它们的 mock 恒定返回 503/429，而我们恒定重试，
  那句 `TurnComplete` 永远不来，表现是测试挂住而不是失败。不改上游那个文件
  （改了等于给足迹加一个上游每次动都会冲突的点，和 precomputed schema blob 同理），
  跑测试时 `-- --skip retry_after` 跳过即可。
- **远端压缩保留上限，但"有本地压缩兜底"这句话曾经是假的。** 这一条原来写着"失败
  有本地压缩兜底"，而上游的分派是**按 provider 能力选路**的：支持远端就走远端，只有
  `Unsupported` 才走本地——本地从来不是失败时的兜底。整个"保留上限"的决定架在一个
  不存在的网上，后果是一次瞬时故障就能掐死一轮（自动压缩之所以发生，正是因为上下文
  已经满了）。2026-08-09 把兜底真的补上了，逻辑在 `core/src/codext_compaction.rs`：
  远端失败 → 先 `report_account_refused()`（这条路**到不了**
  `handle_retryable_response_stream_error`，`compact_remote_v2.rs` 有一句
  `Err(err) if !err.is_retryable() => return Err(err)` 挡在前面，所以那个唯一会上报
  账号的分支从来没执行过）→ 退回本地压缩 → **本地这一趟也无限重试**（上游那个循环
  封顶 5 次、退避几秒，对"模型满载"等于没等）。手动 `/compact` 走同一套。
- **websocket→HTTPS 回退要先判 `err.is_retryable()`。** 上游有个隐含前提：不可重试
  的错误在 `turn.rs` 就被挡掉了，走不到这里。现在它们会走到，而"模型不支持图片输入"
  这种拒绝换条通道重发还是同样被拒——白打一趟不说，本该报给用户的错误会变成一次
  无声的重试。

措辞分两档，因为这两句话让用户做的事完全不同：`Connection to the model failed`
让人去泡杯茶，`will not recover on its own` 让人去看配置。后者按显式变体列表加
4xx 状态码判定，但**只在 websocket 不参与时才信状态码**——一次 WS 升级被 404 掉
说的是"这条通道走不通"，回退到 HTTPS 自己会解决，说成"去改配置"是把人指错方向。

## 为什么均摊同时也是最安全的

一个号被跑满、其余闲着，那个号在对面看来就是台机器；N 个号各跑 1/N，看起来就是
N 个正常用户。安全和公平在这里是同一个方向。

**但调度只能摊，变不出配额。** 总需求超过总供给时任何调度都只是在决定谁先饿死，
这种情况要靠界面把池子余量显性化。

## 配置

环境变量优先，其次 `CODEX_HOME/pool.json`：

```json
{"base_url": "https://pool.example.com:844", "key": "cxk_…"}
```

```
CODEXT_POOL_URL / CODEXT_POOL_KEY / CODEXT_POOL_DEVICE_ID
```

刻意不塞进上游的 `config.toml`——那要改 config crate 的类型定义，每次合并上游都
得重新对一遍。单独一个文件，上游永远不会碰。

两处都没配就什么都不做，退回上游本来的 `auth.json`。所以 codext 是 codex 的严格
超集，装上它不会让任何现有用法失效。

`device_id` 默认是 `主机名-FNV(工作目录)`：同一个项目反复调用复用同一个租约，
不同项目各拿各的号。**不要**改成每台机器一个（那样一台电脑上开不出两个不同的
账号）或者每个进程一个（那样一次 `doctor` 加一次 `exec` 就留下两个幽灵持有者，
抬高调度的并发除数）。

## 上游合并

```
git fetch upstream --tags
git merge rust-v0.<下一个版本>.0        # 基线是 tag，不是 main
```

**发版 tag 之间互相不是祖先。** 上游从 release 分支切 tag，所以上一个 tag 不是下一个
的祖先，合并基点比上一个 tag 还老。workspace `Cargo.toml` 的版本号那行必然冲突（取
上游的）。判据不是"冲突多不多"，是：

```
git diff --cached <新tag>        # 合并结果 vs 纯上游
```

它应当与合并前 `git diff <旧tag> HEAD` 的统计**逐字节相同**。相同 = 既没有上游改动被
盖掉，也没有我们的改动被冲掉。

### 真正的危险是零冲突的那一类

**git 报的冲突不是风险，自动合并成功的才是。** 2026-08-08 合 0.147.0，文本冲突只有
版本号一行，344 个提交全部自动合并——然后有三处坏掉，两处编不过或跑不对，一处**编
得过、跑得通、静默失效**：

1. **`TokenUsage` 加了字段** → `pool_tests.rs` 里整体构造它的 fixture 编不过。编译器
   会告诉你，最轻的一类。

2. **上游新代码读了我们改过的共用分类。** 我们曾把 `ServerOverloaded` 在
   `protocol/src/error.rs` 的 `is_retryable()` 里翻成"可重试"来表达"服务器忙就等"。
   那是把**我们的策略**写进了**全仓共用的分类**——guardian、远端压缩、传输回退、
   app-server 都读它。0.147.0 新增的 guardian 一上来就被绊倒。
   **规则：策略留在 `responses_retry.rs`，不要动 `is_retryable()`。**

3. **挂钩点被"调用图"抛下（最贵的一次，用户先发现的）。** 0.146.0 里
   `shared_from_config` 直接调 `shared()`，我们的 `install_if_configured` 挂在
   `shared()` 上就够。0.147.0 上游在中间插了一层：

   ```
   0.146.0  shared_from_config ──────────────────────► shared() ──► 装 provider ✅
   0.147.0  shared_from_config ──► shared_from_auth_config ──► new_from_auth_config ❌
                                                        （shared() 一个字没改，成了死代码）
   ```

   上游**没动 `shared()`**，动的是**谁调用它**。于是：无冲突、能编译、
   `pool_tests.rs` 全绿（因为它直接调 `shared()`，测的正是那条没人走的路），而
   `codex` 的每一个真实入口（`cli/src/main.rs`、`app-server/src/in_process.rs`、TUI）
   走的都是 `shared_from_config`——**池子永不安装，每次静默退回本机 `auth.json`**。
   表现极具迷惑性：codext 跑起来"跟原生一模一样"，有 `auth.json` 的机器显示旧账号，
   没有的机器让你登录。任何自动化都不会报错。

   **`shared()` 的函数级 churn 确实是 0。churn 分析看不见调用图的变化。**

### 因此每次合并必须做的三件事

1. **跑那条走真实入口的测试**：`pool_tests.rs` 的
   `the_pool_is_installed_on_the_path_the_cli_actually_takes`。它走
   `shared_from_auth_config`（`shared_from_config` 的汇合点，`login` 不能依赖 `core`
   所以只能测到这一层）。**新加挂钩点时，配套的测试必须走真实入口，不能走挂钩点本身。**
2. **核对调用图，不只核对挂钩函数**：
   ```
   grep -rn --include='*.rs' "AuthManager::shared" . | grep -v test
   ```
   出现任何我们没挂过的 `Arc<AuthManager>` 产出路径，就是又一次同样的坑。
3. **做基线对照再下结论**（见"测试"一节）：这个环境下纯上游本来就有 ~42 个失败，
   不对照就分不清哪些是自己弄坏的。

### 挂钩点原则

挂在**汇合点**上，不是挂在"最近没被改过的函数"上。判据是"这条路绕不过去吗"，
不是"这个函数会不会被改"。绕得过去的，就一定有一天会被绕过去。

## 构建

本目录是规范源。**不要在这里 build/test**——`target/` 有好几个 GB，会把 OneDrive
拖垮（`.mwignore` 已排除）。

```
cd <本目录> && mw sync
export PATH=$HOME/.cargo/bin:$PATH        # rustup，让 rust-toolchain.toml 生效
cd ~/.agent-work/runtime/codext-<hash>/codex-rs
cargo test -p codex-login pool
cargo build --release -p codex-cli        # 产物叫 codex，发布时改名 codext
```

**必须走 rustup**（`$HOME/.cargo/bin/cargo`）。直接调 toolchain 目录里的 cargo
会绕过 `rust-toolchain.toml` 的版本解析，本仓库钉的是 1.95.0，用 nightly 编
`codex-tui` 会在 `semicolon_in_expressions_from_macros` 上失败——而 `cargo check`
和 `cargo test` 都不会暴露这个问题。

`mw sync` 之后**核对一下测试数量**：rsync 保留 mtime，DrvFs 的时间戳可能比构建
产物还旧，cargo 会跳过重编。测试数没涨就是没同步上。

## 发布

**codext 的版本号就是它基于的 codex 版本号，一个字符都不加。** `codext
--version` 报 `codex-cli 0.146.0`，因为它就是 0.146.0 的 codex 加了个凭据池。
任何读 codex 版本号的东西——上游的兼容性判断、`ags`、脚本、人——读 codext 必须
得到同样的答案。所以 tag 永远是 `v<上游版本>`：没有 `-2`，没有 `+ours`，没有
任何后缀。

同一个上游基线上再发一版，**不是**开新 tag，是把旧 tag 挪过来重发：

```
git tag -f v0.146.0 <新 commit>
git push -f origin v0.146.0
```

强制推 tag 照样触发 `codext-release.yml`（push 事件对 tag 的更新一样发），
工作流里 `gh release create ... || gh release upload --clobber` 就是为这条路径
写的：release 已经在了就把资产覆盖掉。真没触发就手动 `workflow_dispatch`，
它收一个 tag 参数。

版本号既然不动，`ags update` 就不能靠 tag 判断新旧。它比对的是 release 资产的
sha256 digest（GitHub API 的 `.assets[].digest`），记在
`~/.local/state/ags/codext-release`。字节没变就不重装——重编出一模一样的产物
不会让谁白下一遍。
