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
PICK_HEADROOM = 5%     新派一个号要求的最低余量
KEEP_HEADROOM = 1%     已经在用的号骑到这里才换
```

不是为了防抖（上面已经说明不会抖），是**换号代价不对称**：换一次丢掉整个缓存；
而新派一个只剩 5% 的号，跑几个请求又得再换，缓存代价付两遍。

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

### 配额耗尽不需要终端报告

读数每个请求都在更新，`headroom` 掉到门槛以下调度自己就换号了；窗口重置时
`resets_at` 一过，那个窗被跳过，账号自动回到轮换里。冷却只剩一个用途：终端报
401（上游 `ExternalAuthRefreshReason` 只有 `Unauthorized` 一个变体），服务端把
这个号按下去 180 秒，够它的续期任务跑一轮。

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
