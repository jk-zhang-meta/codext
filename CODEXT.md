# codext

`openai/codex` 的私有分叉。基线 `rust-v0.146.0`，分支 `codext`，上游远端名 `upstream`。

存在的唯一理由：**让 Codex 的凭据来自 PersonalWeb 账号池，而不是本机的
`auth.json`**。一个人跑很多项目，用量摊在若干个账号上，谁都不像机器。

## 上游footprint：两个文件，三个挂钩点

| 文件 | 改动 | 近 5 版上游改动次数 |
| --- | --- | --- |
| `codex-rs/login/src/auth/mod.rs` | `mod pool;` | 1 |
| `codex-rs/login/src/auth/manager.rs` `shared()` | 装 provider | **0** |
| `codex-rs/login/src/auth/manager.rs` `load_auth()` | 池子给不出号时退回本地 | 3 |
| `codex-rs/login/src/auth/manager.rs` 续期分流 | 本地那份走本地续期 | 3 |
| `codex-rs/login/src/auth/pool.rs` | 新文件，全部实现 | — |
| `codex-rs/login/src/auth/pool_tests.rs` | 新文件 | — |

没有新增依赖，没有动 `Cargo.toml`。

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
- 续期按**手上这份是谁发的**分流，不能按"装没装 provider"。退回的那份是本机
  `CodexAuth::Chatgpt`，自带 refresh token，只能走上游的本地续期；按 provider 判
  的话它会被送去问池子，而池子正是刚才给不出号的那个。判据是
  `!matches!(auth, CodexAuth::Chatgpt(_))`——bearer provider 发的 `Headers` 凭据
  仍归 provider 续，别收窄成 `is_external_chatgpt_tokens()`，那会打断上游的 bearer
  路径（有测试盯着）。

主动续期没做：`auth()` 在 provider 模式下跳过 `should_refresh_proactively`，所以
退回本地之后那份凭据要等一次 401 才被动续期。多一个往返，够用了。

### 配额耗尽：先靠读数提前换，撞上了再换号重试

**正常路径不需要终端报告。** 读数每个请求都在更新，`headroom` 掉到门槛以下调度
自己就换号了；窗口重置时 `resets_at` 一过，那个窗被跳过，账号自动回到轮换里。

但读数是**上一次调用**留下的，天然滞后一到两档，而多台机器共用一个号时还会被
别人的消耗甩开。所以真撞上 429 时还有第二道：`UsageLimitReached` 不再终结这一轮，
而是落进重试循环——重试会重新取一次凭据，池子据此换一个还有余量的号；没有池子时
就按自己那个窗口重置的节奏慢慢等。这条路上有三个必须知道的约束：

- **重试至少等 6 秒**（`ACCOUNT_SWAP_MIN_DELAY`）。换号靠的是重试时重新取凭据，而
  池子判断依据是终端扫 rollout 报上去的读数，那个扫描节流 5 秒。等不够就重试，报
  上去是空的，池子把同一个号再派回来。改 `USAGE_SCAN_MIN_INTERVAL` 时这里要跟着改。
- **换号时必须丢掉 websocket 连接**（`client.rs`）。上游只有"401 后续期"这一种换
  凭据的情形，账号身份不变，所以连接一直是复用的；租借模式下账号会变，而连接还开
  着就不会用新凭据重连——不显式重置的话，换号在 websocket 传输上完全不生效。连带
  要丢掉 `previous_response_id` 和 turn state 头，它们都是按账号绑定的。
- **池子一个号都发不出来时**（`pool.rs` 的 `POOL_EXHAUSTED` / `EXHAUSTION_ANNOUNCED`）
  报一次"去后台加号"，然后每 30 秒回去问一次。通报是边沿触发的：多个会话同时撞上
  只报一条，恢复供号时标记清掉，下一次枯竭照样报。

`QuotaExceeded` **不**算账号级：它是计费状态（"检查你的套餐和账单"），不随窗口重置
恢复，也不带额度读数，换号救不了它——它归到"不会自己好"那一档，照样重试，但话要
说成"去把账单修好"。

冷却只剩一个用途：终端报 401（上游 `ExternalAuthRefreshReason` 只有 `Unauthorized`
一个变体），服务端把这个号按下去 180 秒，够它的续期任务跑一轮。

### 会话由用户结束，不由错误结束

`responses_retry.rs`。上游在重试循环里问的是"再试一次有没有希望"，答案是否就结束
这一轮。codext 问的是另一个问题：**这一轮该不该结束**。断开一个跑到一半的会话，
代价永远高于多等一会儿——能修的错误（模型名写错、欠费、代理配错）用户看到提示以后
可以去修，修好了下一次重试就通了；修不了的，用户按 Esc。两条路都比替他做决定强。

所以采样路径上**没有次数上限**，只有五个出口，而且没有一个是"我们判断重试没用"：

| 出口 | 为什么它不能重试 |
|---|---|
| `TurnAborted` / `Interrupted` | 这**就是**用户按下的暂停。重试它们等于让 Esc 失灵，而"随时可以自己叫停"正是无限重试能成立的前提 |
| `ContextWindowExceeded` | 同一个超长请求重试一万次还是装不下，用户在一轮**之内**也没法把它变短 |
| `SessionBudgetExceeded` | 用户自己设的开销上限，绕过它等于把这个设置悄悄作废 |
| `CyberPolicy` | 安全策略拒绝是**答复**，不是故障。把一个安全判定塞进循环里反复问，性质上就不该由客户端自动做 |

其余一切都无限重试。分类只决定**等多久、怎么说**（`RetryKind`），不决定要不要重试：
池子枯竭 30 秒一次、换号 6–60 秒、没池子等窗口重置 60–600 秒、其余走上游退避曲线
封顶 60 秒。每一类**只报一次**——每隔几十秒刷同一句话等于把提示自己淹掉，而一个
不断增长的 attempt 计数看起来像是坏了，不像在等。

三个容易踩的地方：

- **`stream_max_retries` 现在是退出开关。** 没设过 = 永不放弃；设过 = 一字不差回到
  上游（退避曲线、日志、"Reconnecting... n/m" 措辞全都是）。这样"我不想要这个行为"
  是一句配置的事，也让上游那批钉着次数的测试原样通过。但**账号级失败和池子枯竭不
  受这个开关约束**——那两件事是这套东西存在的理由。
- **远端压缩保留上限。** 它是一轮**里面**的一步，不是会话本身，而且失败有本地压缩
  兜底。在那里无限等会把整轮挂死，还顺手挡掉兜底，比报错更糟。
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
{"base_url": "https://www.itachi.fans:844", "key": "cxk_…"}
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

冲突面应当只有 `shared()` 那几行。如果冲突扩散到别处，说明有改动溢出了
`pool.rs` 的边界，需要收回去。

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
