//! 从自建的账号池服务在线租借 Codex 凭据，替代本机 `auth.json`。
//!
//! 挂在上游的 `ExternalAuth` 扩展点上。`AuthManager::load_auth()` 会优先问这里，
//! 而 `auth()` 每次取凭据都会重新 resolve 一遍，所以换号既不用动任何文件、也不
//! 受「`CODEX_HOME` 启动后不可更改」的限制——同一台机器上的多个进程各租各的号。
//!
//! **每个请求都向池子问一次该用哪个号，本地不做缓存判断。** 稳定性由服务端的调度
//! 算法保证（手上那个还能用就原样还回来，绝不为"别的号更宽裕"而换），不是靠客户端
//! 攥着不放——那样额度跑满了也换不掉。同一趟往返顺带把用量和额度读数带上去，服务端
//! 因此总是拿着上一次响应的真实读数在做决定。
//!
//! refresh token 永远留在服务端：`CodexAuth::from_external_chatgpt_tokens` 造出
//! 来的凭据本来就不带它，续期由服务端独占。多台机器各自拿着同一个 refresh token
//! 去刷新，只会把彼此的令牌轮换作废。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use codex_http_client::HttpClient;
use codex_utils_path::write_atomically;
use serde::Deserialize;
use serde::Serialize;

use super::default_client::create_client;
use super::manager::AuthManager;
use super::manager::CodexAuth;
use super::manager::ExternalAuth;
use super::manager::ExternalAuthFuture;
use super::manager::ExternalAuthRefreshContext;

/// 路径前缀沿用服务端那套不可猜测的形式，理由相同：这个端点不需要被发现。
const PATH_PREFIX: &str = "/x8Rk3Nq6Vd2";
const TOKEN_HEADER: &str = "X-Codex-Pool-Token";

/// 单次派号最多等多久。
///
/// 每个请求都要过这一趟，所以服务端卡住不能把 codext 一起卡住：超时之后退回手上
/// 那份凭据（通常还有几十分钟有效期），会话照跑。
const POOL_TIMEOUT: Duration = Duration::from_secs(3);

/// 把一次模型调用前后的连锁取凭据合并成一次派号。
///
/// **这不是租约缓存。** 上游的 `AuthManager::auth()` 在外部 provider 模式下每次都
/// 走一遍 `reload()`，而围绕一次模型调用它会被调几十次（实测启动阶段 550 毫秒里
/// 十几次）。不合并的话一个请求要打几十个往返，服务端被当成心跳靶子。
///
/// 窗口取 1 秒是有依据的，不是随手定的：决策**只可能**在有新信息时改变，而新信息
/// 最快也要 [`USAGE_SCAN_MIN_INTERVAL`]（5 秒）才来一批。真正的模型调用间隔以秒
/// 计，所以每个请求依然拿到一次全新的决策——被合并掉的全是同一个请求内部的重复
/// 提问。原来那套是 200 秒，差着两个数量级。
///
/// 401 不走这条路：`refresh()` 永远重新问，手上那份刚被对面拒绝。
const DECISION_COALESCE: Duration = Duration::from_secs(1);

/// 两次扫 rollout 之间至少隔这么久。
///
/// 派号是每个请求一次，扫文件没必要跟着这么密——同一个响应写进 rollout 之后，
/// 隔几秒扫到和立刻扫到没有区别，而每个请求都重读一遍几 MB 的会话文件有。
const USAGE_SCAN_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// 没有请求的时候多久主动找一次池子。
///
/// 派号本身挂在请求路径上，安静的时候不需要它。这个心跳是为了让一个开着不动的
/// 会话也能把读数报上去，顺带把租约续上。
const IDLE_TICK: Duration = Duration::from_secs(20);

/// 往回扫多久之内动过的 rollout。
///
/// **不能只扫"本次运行开始之后"的。** 一次 `codex exec` 的最后一次响应写完
/// rollout 就退出了，没有下一个取凭据的调用会把它带上去——只报本次运行的话，
/// 每次运行都会丢掉最后一轮，而一个只跑一轮的 `codex exec` 就等于**永远报不出
/// 任何读数**。读数报不出来，服务端对每个号都只能假设满余量，调度直接退化。
///
/// 所以扫最近这段时间，让下一次运行把上一次的尾巴带上去。归属靠
/// [`ATTRIBUTION_FILE`] 记着，不会算到错误的号头上。
const REPORT_WINDOW_SECONDS: u64 = 6 * 3600;

/// 一次最多扫多少个 rollout。够覆盖最近几次运行摸过的会话，又不至于在一个跑了很久
/// 的 `CODEX_HOME` 上翻几千个文件。
const MAX_ROLLOUTS: usize = 20;

/// 会话到账号的归属记录。
///
/// 用量的唯一键是 (session_id, account_key)，所以把一个会话记到错误的号头上不是
/// 覆盖而是**多出一行**，两个号各背一份完整用量。跨运行上报必须知道当初是谁服务
/// 的，这个文件就是干这个的。
const ATTRIBUTION_FILE: &str = "pool-sessions.json";
const ATTRIBUTION_LOCK_FILE: &str = "pool-sessions.lock";

/// 归属表最多留多少条。够覆盖最近几天，又不至于无限长下去。
const MAX_ATTRIBUTIONS: usize = 500;

/// v2 表示终端上报的是每个账号实际服务段的增量快照。
const USAGE_COUNTER_VERSION: u8 = 2;

/// 我们自己那份配置在 `CODEX_HOME` 下的文件名。
///
/// 刻意不塞进上游的 `config.toml`：那要改 config crate 的类型定义，每次合并上游
/// 都得重新对一遍。单独一个文件，上游永远不会碰。
const CONFIG_FILE: &str = "pool.json";

/// 服务端明确回了「此刻没号可发」。
///
/// 这是唯一一个**需要人介入**的状态：调度只能把负载摊开，变不出配额。所以它不能
/// 悄悄退回本地号（让人以为一切正常），也不能把会话打断（让人以为是网络问题）——
/// 必须显性地出现在终端上，然后挂在那里等，直到后台加了号。
static POOL_EXHAUSTED: AtomicBool = AtomicBool::new(false);

/// 这一轮枯竭有没有通报过。
///
/// 等待是无限的，但话只说一次：每个请求都刷一遍"没号了"，等于把真正的提示淹掉。
static EXHAUSTION_ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// 池子此刻发不出号。
pub fn pool_is_exhausted() -> bool {
    POOL_EXHAUSTED.load(Ordering::Relaxed)
}

/// 本轮枯竭还没通报过就返回 true，并就地标记为已通报。
///
/// 用 `compare_exchange` 而不是「读了再写」：同一个进程里可能有多个会话同时撞上
/// 枯竭，读写分开的话它们会各报一次。
pub fn take_pool_exhaustion_notice() -> bool {
    POOL_EXHAUSTED.load(Ordering::Relaxed)
        && EXHAUSTION_ANNOUNCED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

fn mark_pool_exhausted() {
    if !POOL_EXHAUSTED.swap(true, Ordering::SeqCst) {
        tracing::warn!("codext: the pool has no account left to hand out");
    }
}

/// 派到号了。连通报标记一起清掉，下一次枯竭会重新报一次。
fn mark_pool_serving() {
    if POOL_EXHAUSTED.swap(false, Ordering::SeqCst) {
        EXHAUSTION_ANNOUNCED.store(false, Ordering::SeqCst);
        tracing::info!("codext: the pool is handing out accounts again");
    }
}

/// 配了池子就接管凭据来源；没配就什么都不做，codext 退回上游原本的 auth.json。
///
/// 配了也不等于绑死：池子给不出号的时候 `AuthManager::load_auth` 会退回本机
/// auth.json，见那里的注释。
pub(super) async fn install_if_configured(manager: &Arc<AuthManager>, codex_home: &Path) {
    let Some(config) = Config::load(codex_home) else {
        return;
    };
    tracing::info!(
        "codext: leasing credentials from the pool as device {}",
        config.device_id
    );
    let pool = Arc::new(PoolAuth::new(config, codex_home.to_path_buf()));
    // 装不上（池子不可达、密钥不对）不该让 codext 起不来：留在本地认证上，
    // 用户至少还能用自己 `codex login` 登过的号。
    if let Err(err) = manager.set_external_auth(pool.clone()).await {
        tracing::warn!("codext: pool auth unavailable, keeping local auth: {err}");
        return;
    }
    spawn_idle_tick(pool);
}

/// 安静的时候也定期找一次池子。
///
/// 派号本身在请求路径上，这个心跳只解决"最后一轮没人带上去"：见 [`IDLE_TICK`]。
/// 走的是同一个 [`PoolAuth::current`]，没有第二套上报逻辑。
fn spawn_idle_tick(pool: Arc<PoolAuth>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(IDLE_TICK).await;
            if let Err(err) = pool.current(/*allow_stale*/ true).await {
                // 池子不可达不影响会话继续跑，降级成调试日志。
                tracing::debug!("codext: idle tick failed: {err}");
            }
        }
    });
}

struct Config {
    base_url: String,
    key: String,
    device_id: String,
}

/// `CODEX_HOME/pool.json`：`{"base_url": "https://…:844", "key": "…"}`
#[derive(Deserialize)]
struct StoredConfig {
    base_url: String,
    key: String,
}

impl Config {
    /// 环境变量优先，其次 `CODEX_HOME/pool.json`。
    ///
    /// 常规路径是**文件**：`ags codex-init` 把地址和密钥一次性写进
    /// `CODEX_HOME/pool.json`，之后 ags 启动 codext 时只是继承环境，不传任何池子
    /// 的环境变量。环境变量放在前面是留一个一次性覆盖的口子（临时换个池子、CI），
    /// 不是常规来源。两处都没有就返回 None。
    fn load(codex_home: &Path) -> Option<Self> {
        let stored = std::fs::read_to_string(codex_home.join(CONFIG_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str::<StoredConfig>(&raw).ok())
            .filter(|stored| !stored.base_url.trim().is_empty() && !stored.key.trim().is_empty());

        let base_url = env_non_empty("CODEXT_POOL_URL").or_else(|| {
            stored
                .as_ref()
                .map(|stored| stored.base_url.trim().to_string())
        })?;
        let key = env_non_empty("CODEXT_POOL_KEY")
            .or_else(|| stored.as_ref().map(|stored| stored.key.trim().to_string()))?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            key,
            // device_id 默认按**工作目录**取，不是每台机器一个、也不是每个进程
            // 一个。服务端按 device_id 认租约，同一个 id 会拿回同一个号：
            //
            // - 每台机器一个 → 一台机器上开不出两个不同的账号，而那正是这套东西
            //   存在的理由（用户的原话：哪怕一套电脑也要能用不同的 auth）。
            // - 每个进程一个 → 每次调用都是一个新租约，而租约要挂满 TTL 才过期。
            //   一次 `doctor` + 一次 `exec` 就留下两个幽灵持有者，它们会**抬高
            //   调度的并发除数**，反过来把派号算歪。
            //
            // 按工作目录取则两头都对：同一个项目反复调用复用同一个租约，不同项目
            // 各拿各的号。
            device_id: env_non_empty("CODEXT_POOL_DEVICE_ID")
                .unwrap_or_else(|| format!("{}-{}", host_label(), workspace_tag())),
        })
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn host_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "codext".to_string())
}

/// 当前工作目录的短稳定标记。
///
/// 只取摘要不取路径本身：路径会带用户名和项目名，而 device_id 是要发到服务端、
/// 显示在后台界面上的。
fn workspace_tag() -> String {
    let workspace = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    // FNV-1a，够用且不用引依赖。这不是安全边界，只是要一个稳定的短名字。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in workspace.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

struct Lease {
    account_key: String,
    auth: CodexAuth,
    decided_at: Instant,
}

/// 上次扫 rollout 的节流状态。
struct Scan {
    /// 本次运行开始的时刻。决定一个没有归属记录的会话能不能算我们的——见
    /// [`collect_sessions`] 第 3 条。
    started: SystemTime,
    next: Instant,
    saw_current_rollout: bool,
}

#[derive(Default)]
struct ScanOutcome {
    sessions: Vec<SessionUsage>,
    saw_current_rollout: bool,
}

struct PoolAuth {
    config: Config,
    client: HttpClient,
    codex_home: PathBuf,
    /// 手上这份凭据。**只有两个用途**：告诉服务端"我现在用的是哪个号"，以及在
    /// 池子不可达时兜底。它不做决策短路——每个请求都要重新派一次号，手上这个还
    /// 能不能接着用由服务端判定。
    lease: RwLock<Option<Lease>>,
    scan: RwLock<Scan>,
}

impl PoolAuth {
    fn new(config: Config, codex_home: PathBuf) -> Self {
        Self {
            config,
            client: create_client(),
            codex_home,
            lease: RwLock::new(None),
            scan: RwLock::new(Scan {
                started: SystemTime::now(),
                next: Instant::now(),
                saw_current_rollout: false,
            }),
        }
    }

    /// 派一次号。每次取凭据都会走这里，没有缓存短路。
    ///
    /// `allow_stale` 决定池子联系不上时能不能拿手上那份顶着用。取凭据时可以
    /// （access token 通常还有十几分钟有效期，一次网络抖动不该打断会话）；凭据
    /// 刚被 401 拒绝时不行——那份已经被对面拒了，再用一次只会再失败一次。
    async fn current(&self, allow_stale: bool) -> std::io::Result<CodexAuth> {
        let recent = allow_stale.then(|| self.recent_decision()).flatten();
        if recent.is_some() && self.saw_current_rollout() {
            return Ok(recent.expect("checked above"));
        }
        let held = self.held_account();
        // 没有 held 也要扫：上次进程退出前的最后一轮，要按持久化归属补报。
        let outcome = self.scan_usage(held.as_deref(), false, None).await?;
        if let Some(auth) = recent
            && !outcome.saw_current_rollout
        {
            return Ok(auth);
        }
        let reject = (!allow_stale).then_some(REJECT_UNAUTHORIZED);
        match self.post(held.as_deref(), reject, outcome.sessions).await {
            Ok(Some(data)) => {
                let account_key = data.account_key.clone();
                let auth = match Self::validate(&data) {
                    Ok(auth) => auth,
                    Err(err) => {
                        self.abandon_current(held.as_deref()).await?;
                        return Err(err);
                    }
                };
                mark_pool_serving();
                if held.as_deref() != Some(data.account_key.as_str()) {
                    // 常规扫描可能正处在 5 秒节流窗内。切号前强制结清旧账号，随后
                    // 原子记下新账号；否则这几秒的尾巴会在下一轮被记到新号头上。
                    self.scan_usage(held.as_deref(), true, Some(Some(data.account_key.as_str())))
                        .await?;
                }
                Ok(self.store(account_key, auth))
            }
            // 「此刻没号可发」是确定的答复，不是抖动：手上那份多半正是服务端刚决定
            // 不再发的那个号（额度到顶、被停用、被冷却），继续骑着只会一路撞墙。所以
            // 这里**不复用**旧租约，报上去让 `AuthManager::load_auth` 退回本机
            // auth.json；下一个请求照样会再问池子一次，号一回来就自动切回去。
            Ok(None) => {
                mark_pool_exhausted();
                self.abandon_current(held.as_deref()).await?;
                Err(std::io::Error::other("no account available in the pool"))
            }
            Err(err) if allow_stale => match self.cached_auth() {
                Some(auth) => {
                    tracing::warn!("codext: pool unreachable, reusing the current lease: {err}");
                    Ok(auth)
                }
                None => {
                    self.abandon_current(held.as_deref()).await?;
                    Err(err)
                }
            },
            Err(err) => {
                self.abandon_current(held.as_deref()).await?;
                Err(err)
            }
        }
    }

    async fn abandon_current(&self, held: Option<&str>) -> std::io::Result<()> {
        let result = self.scan_usage(held, true, Some(None)).await;
        self.forget_lease();
        result.map(|_| ())
    }

    /// 扫一遍最近的 rollout，取出用量和额度读数，并记下每个会话归谁。
    ///
    /// 节流到 [`USAGE_SCAN_MIN_INTERVAL`]：派号是每个请求一次，重读几 MB 的会话
    /// 文件没必要跟着这么密。服务端按 (session_id, account_key) 去重、计数取较大
    /// 值，所以少报一轮或重复报都无害。
    async fn scan_usage(
        &self,
        held: Option<&str>,
        force: bool,
        transition: Option<Option<&str>>,
    ) -> std::io::Result<ScanOutcome> {
        let started = {
            let Ok(mut guard) = self.scan.write() else {
                return Err(std::io::Error::other("usage scan lock is poisoned"));
            };
            let now = Instant::now();
            if !force && guard.saw_current_rollout && now < guard.next {
                return Ok(ScanOutcome::default());
            }
            guard.next = now + USAGE_SCAN_MIN_INTERVAL;
            guard.started
        };
        let home = self.codex_home.clone();
        let held = held.map(str::to_string);
        let transition = transition.map(|account| account.map(str::to_string));
        // 扫目录和读文件都是阻塞 IO，不能直接压在异步线程上。
        let outcome = tokio::task::spawn_blocking(move || {
            try_collect_sessions(
                &home,
                held.as_deref(),
                started,
                transition.as_ref().map(|account| account.as_deref()),
            )
        })
        .await
        .map_err(std::io::Error::other)??;
        let mut guard = self
            .scan
            .write()
            .map_err(|_| std::io::Error::other("usage scan lock is poisoned"))?;
        if outcome.saw_current_rollout {
            guard.saw_current_rollout = true;
        } else {
            // A startup scan before the rollout exists must not consume the
            // five-second window; the next auth lookup needs to claim it.
            guard.next = Instant::now();
        }
        Ok(outcome)
    }

    /// 向池子要一个号。`Ok(None)` 是「此刻没号可发」——那是服务端的正常答复，
    /// 和「联系不上」不是一回事，两者的处置也不一样，见 [`PoolAuth::current`]。
    async fn post(
        &self,
        account_key: Option<&str>,
        reject: Option<&str>,
        sessions: Vec<SessionUsage>,
    ) -> std::io::Result<Option<LeaseData>> {
        let url = format!("{}{PATH_PREFIX}/lease", self.config.base_url);
        let response = self
            .client
            .post(url.as_str())
            .header(TOKEN_HEADER, self.config.key.as_str())
            .header("Content-Type", "application/json")
            .timeout(POOL_TIMEOUT)
            .json(&PoolRequest {
                device_id: &self.config.device_id,
                account_key,
                reject,
                sessions,
            })
            .send()
            .await
            .map_err(std::io::Error::other)?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(std::io::Error::other(format!(
                "pool lease failed: {status}: {body}"
            )));
        }
        let envelope: Envelope = response.json().await.map_err(std::io::Error::other)?;
        if envelope.code != 0 {
            return Err(std::io::Error::other(format!(
                "pool lease rejected: {}",
                envelope.message.unwrap_or_default()
            )));
        }
        Ok(envelope.data)
    }

    fn validate(data: &LeaseData) -> std::io::Result<CodexAuth> {
        let account_id = data.auth_json.tokens.account_id.as_deref().ok_or_else(|| {
            std::io::Error::other("pool returned credentials without a chatgpt account id")
        })?;
        CodexAuth::from_external_chatgpt_tokens(
            &data.auth_json.tokens.access_token,
            account_id,
            data.plan.as_deref(),
        )
    }

    fn store(&self, account_key: String, auth: CodexAuth) -> CodexAuth {
        if let Ok(mut guard) = self.lease.write() {
            *guard = Some(Lease {
                account_key,
                auth: auth.clone(),
                decided_at: Instant::now(),
            });
        }
        auth
    }

    fn saw_current_rollout(&self) -> bool {
        self.scan
            .read()
            .map(|guard| guard.saw_current_rollout)
            .unwrap_or(false)
    }

    /// 刚刚才决定过的话就用那次的结果。见 [`DECISION_COALESCE`]。
    fn recent_decision(&self) -> Option<CodexAuth> {
        let guard = self.lease.read().ok()?;
        let lease = guard.as_ref()?;
        (lease.decided_at.elapsed() < DECISION_COALESCE).then(|| lease.auth.clone())
    }

    fn cached_auth(&self) -> Option<CodexAuth> {
        let guard = self.lease.read().ok()?;
        guard.as_ref().map(|lease| lease.auth.clone())
    }

    /// 服务端不打算再发手上这个号了，忘掉它。
    ///
    /// 留着的话下一个请求还会把它当作「我手上的号」报上去，连带把**退回本地之后**
    /// 跑掉的用量和额度读数记到它头上——那是调度赖以判断的读数，污染不得。
    fn forget_lease(&self) {
        if let Ok(mut guard) = self.lease.write() {
            *guard = None;
        }
    }

    fn held_account(&self) -> Option<String> {
        let guard = self.lease.read().ok()?;
        guard.as_ref().map(|lease| lease.account_key.clone())
    }
}

impl ExternalAuth for PoolAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(self.current(/*allow_stale*/ true))
    }

    /// 上游只在一种情况下调这里：401。
    ///
    /// 服务端提前 15 分钟续期、租约只有 10 分钟，所以派出去的 token 不该是过期的
    /// ——真收到 401 说明这个号此刻用不了。带上 `reject` 让服务端把它短暂按下去
    /// （够续期任务跑一轮），顺便换一个号回来。
    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(self.current(/*allow_stale*/ false))
    }
}

/// 上游的 `ExternalAuthRefreshReason` 只有 `Unauthorized` 一个变体，所以报上去的
/// 理由也只有这一个。配额耗尽走不到这里：那个由每个请求带回来的读数自己体现。
const REJECT_UNAUTHORIZED: &str = "unauthorized";

#[derive(Serialize)]
struct PoolRequest<'a> {
    device_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reject: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<SessionUsage>,
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    data: Option<LeaseData>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct LeaseData {
    account_key: String,
    #[serde(default)]
    plan: Option<String>,
    auth_json: AuthJson,
}

#[derive(Deserialize)]
struct AuthJson {
    tokens: Tokens,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Serialize, Default, Debug, PartialEq)]
struct SessionUsage {
    session_id: String,
    /// v2 按每次调用的增量计数；服务端据此覆盖 v1 留下的累计基线虚高值。
    counter_version: u8,
    /// 这个会话归哪个号。省略时服务端按请求上的 `account_key` 算。
    #[serde(skip_serializing_if = "Option::is_none")]
    account_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    requests: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimits>,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
struct UsageCounters {
    requests: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
}

impl UsageCounters {
    fn from_usage(usage: &SessionUsage) -> Self {
        Self {
            requests: usage.requests,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            total_tokens: usage.total_tokens,
        }
    }

    fn saturating_sub(self, previous: Self) -> Self {
        Self {
            requests: self.requests.saturating_sub(previous.requests),
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_tokens: self
                .reasoning_tokens
                .saturating_sub(previous.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
        }
    }

    fn add_assign(&mut self, delta: Self) {
        self.requests = self.requests.saturating_add(delta.requests);
        self.input_tokens = self.input_tokens.saturating_add(delta.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(delta.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(delta.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(delta.total_tokens);
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct SessionAttribution {
    active_account: Option<String>,
    #[serde(default)]
    last_seen: UsageCounters,
    #[serde(default)]
    accounts: BTreeMap<String, UsageCounters>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum StoredAttribution {
    Legacy(String),
    Current(SessionAttribution),
}

#[derive(Serialize, Clone, Debug, PartialEq)]
struct RateLimits {
    observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    windows: Vec<RateWindow>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
struct RateWindow {
    window_minutes: u64,
    used_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<i64>,
}

/// 最近动过的会话，按真正服务它的账号拆成完整累计快照。
///
/// `transition` 为 `Some` 时是一次账号切换：先把新增用量结给旧的
/// `active_account`，再在同一次文件写回里切到新账号。`Some(None)` 表示退回本地
/// auth；这期间只推进基线，不把用量记到任何池账号。
fn try_collect_sessions(
    codex_home: &Path,
    held: Option<&str>,
    started: SystemTime,
    transition: Option<Option<&str>>,
) -> std::io::Result<ScanOutcome> {
    let lock_path = codex_home.join(ATTRIBUTION_LOCK_FILE);
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock_file.lock()?;

    let mut attributions = load_attribution(codex_home)?;
    let mut seen = Vec::new();
    let mut sessions = Vec::new();
    let mut saw_current_rollout = false;
    for (modified, path) in recent_rollouts(codex_home) {
        let is_current_rollout = modified >= started;
        saw_current_rollout |= is_current_rollout;
        let session_id = rollout_session_id(&path);
        if session_id.is_empty() {
            continue;
        }
        let usage = read_rollout(&path);
        let stored = attributions.remove(&session_id);
        let (mut attribution, migrated_legacy) = match stored {
            Some(StoredAttribution::Current(attribution)) => (attribution, false),
            Some(StoredAttribution::Legacy(owner)) => {
                let mut accounts = BTreeMap::new();
                let last_seen = usage
                    .as_ref()
                    .map(UsageCounters::from_usage)
                    .unwrap_or_default();
                if usage.is_some() {
                    accounts.insert(owner.clone(), last_seen);
                }
                (
                    SessionAttribution {
                        active_account: Some(owner),
                        last_seen,
                        accounts,
                    },
                    true,
                )
            }
            None if is_current_rollout && (held.is_some() || transition.is_some()) => (
                SessionAttribution {
                    active_account: held.map(str::to_string),
                    last_seen: UsageCounters::default(),
                    accounts: BTreeMap::new(),
                },
                false,
            ),
            None => continue,
        };
        seen.push(session_id.clone());

        let rate_limit_account = attribution.active_account.clone();
        if let Some(usage) = usage.as_ref()
            && !migrated_legacy
        {
            let current = UsageCounters::from_usage(usage);
            let delta = current.saturating_sub(attribution.last_seen);
            if let Some(owner) = attribution.active_account.as_ref() {
                attribution
                    .accounts
                    .entry(owner.clone())
                    .or_default()
                    .add_assign(delta);
            }
            attribution.last_seen = current;
        }

        if let Some(next_account) = transition
            && is_current_rollout
            && (held.is_none() || attribution.active_account.as_deref() == held)
        {
            attribution.active_account = next_account.map(str::to_string);
        } else if migrated_legacy && held.is_some() && is_current_rollout {
            // 旧文件只有历史 owner；完整旧用量留给它，从现在开始的新 delta 归当前号。
            attribution.active_account = held.map(str::to_string);
        }

        let Some(usage) = usage else {
            attributions.insert(session_id, StoredAttribution::Current(attribution));
            continue;
        };
        for (account_key, counters) in &attribution.accounts {
            sessions.push(SessionUsage {
                session_id: session_id.clone(),
                counter_version: USAGE_COUNTER_VERSION,
                account_key: Some(account_key.clone()),
                model: usage.model.clone(),
                requests: counters.requests,
                input_tokens: counters.input_tokens,
                cached_input_tokens: counters.cached_input_tokens,
                output_tokens: counters.output_tokens,
                reasoning_tokens: counters.reasoning_tokens,
                total_tokens: counters.total_tokens,
                rate_limits: if rate_limit_account.as_deref() == Some(account_key.as_str()) {
                    usage.rate_limits.clone()
                } else {
                    None
                },
            });
        }
        attributions.insert(session_id, StoredAttribution::Current(attribution));
    }
    save_attribution(codex_home, &attributions, &seen)?;
    Ok(ScanOutcome {
        sessions,
        saw_current_rollout,
    })
}

#[cfg(test)]
fn collect_sessions(
    codex_home: &Path,
    held: Option<&str>,
    started: SystemTime,
    transition: Option<Option<&str>>,
) -> Vec<SessionUsage> {
    try_collect_sessions(codex_home, held, started, transition)
        .expect("collect pool sessions")
        .sessions
}

/// 最近动过的 rollout，最新的在前，带上各自的修改时间。
fn recent_rollouts(codex_home: &Path) -> Vec<(SystemTime, PathBuf)> {
    let since = SystemTime::now()
        .checked_sub(Duration::from_secs(REPORT_WINDOW_SECONDS))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut found = Vec::new();
    collect_rollouts(&codex_home.join("sessions"), since, 0, &mut found);
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.truncate(MAX_ROLLOUTS);
    found
}

fn attribution_path(codex_home: &Path) -> PathBuf {
    codex_home.join(ATTRIBUTION_FILE)
}

fn load_attribution(codex_home: &Path) -> std::io::Result<HashMap<String, StoredAttribution>> {
    let raw = match std::fs::read_to_string(attribution_path(codex_home)) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err),
    };
    serde_json::from_str(&raw).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid pool session attribution: {err}"),
        )
    })
}

/// 存回归属表，只留最近扫到的那些。
///
/// 按「这一批扫到了什么」裁剪而不是按条数裁：扫的范围本来就是最近这段时间，超出
/// 那个范围的会话再也不会被上报，留着只会让文件无限长下去。
fn save_attribution(
    codex_home: &Path,
    attributions: &HashMap<String, StoredAttribution>,
    seen: &[String],
) -> std::io::Result<()> {
    let kept: BTreeMap<&str, &StoredAttribution> = seen
        .iter()
        .take(MAX_ATTRIBUTIONS)
        .filter_map(|session_id| {
            attributions
                .get(session_id)
                .map(|attribution| (session_id.as_str(), attribution))
        })
        .collect();
    let body = serde_json::to_string(&kept).map_err(std::io::Error::other)?;
    write_atomically(&attribution_path(codex_home), &body)
}

fn collect_rollouts(
    dir: &Path,
    since: SystemTime,
    depth: usize,
    out: &mut Vec<(SystemTime, PathBuf)>,
) {
    // 布局是 sessions/YYYY/MM/DD/rollout-*.jsonl，给一层余量。
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        if meta.is_dir() {
            collect_rollouts(&path, since, depth + 1, out);
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
            continue;
        }
        // 本次运行开始之后没再动过的，要么早报过、要么根本属于别的账号。
        if let Ok(modified) = meta.modified()
            && modified >= since
        {
            out.push((modified, path));
        }
    }
}

/// 读一个 rollout 的用量。
///
/// Codex 每次模型调用写一条 `token_count` 事件，`info` 里既有 `total_token_usage`
/// （线程累计）也有 `last_token_usage`（这一次）。fork 出来的子线程会继承父线程的
/// 累计值，所以现代 rollout 必须把每条 `last_token_usage` 相加；取最后一条累计值会
/// 把父线程的整段历史在每个子线程里再算一遍。
///
/// 同一次调用偶尔会落两条完全相同的累计快照，请求数和增量都只算一次。没有
/// `last_token_usage` 的旧 rollout 才退回最后一条累计值。
fn read_rollout(path: &Path) -> Option<SessionUsage> {
    let session_id = rollout_session_id(path);
    if session_id.is_empty() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut usage = SessionUsage {
        session_id,
        counter_version: USAGE_COUNTER_VERSION,
        ..SessionUsage::default()
    };
    let mut seen = false;
    let mut last_cumulative = None;
    let mut incremental = [0_u64; 5];
    let mut all_events_have_incremental = true;
    let mut saw_incremental = false;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("token_count") && !line.contains("turn_context") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = event.get("payload").unwrap_or(&event);
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .or_else(|| payload.get("type").and_then(serde_json::Value::as_str));
        if event_type == Some("turn_context")
            && let Some(model) = payload.get("model").and_then(serde_json::Value::as_str)
        {
            usage.model = Some(model.to_string());
        }
        if payload.get("type").and_then(serde_json::Value::as_str) != Some("token_count") {
            continue;
        }
        if let Some(model) = payload.get("model").and_then(serde_json::Value::as_str) {
            usage.model = Some(model.to_string());
        }
        // 留最后一条读数：最新的那次调用才反映账号此刻的额度。
        if let Some(limits) = read_rate_limits(payload, &event) {
            usage.rate_limits = Some(limits);
        }
        // OpenAI 会单独落一条 `info: null` 的额度刷新事件。它不是模型调用，不能
        // 增加请求数、更不能把现代 rollout 降级到含父线程基线的累计口径。
        let Some(info) = payload.get("info").filter(|value| value.is_object()) else {
            continue;
        };
        seen = true;
        if let Some(model) = info.get("model").and_then(serde_json::Value::as_str) {
            usage.model = Some(model.to_string());
        }
        // 相同累计快照是同一次调用的重复落盘。额度读数仍取最新，但用量不重算。
        let totals = info.get("total_token_usage").unwrap_or(info);
        let cumulative = token_counts(totals);
        if last_cumulative == Some(cumulative) {
            continue;
        }
        last_cumulative = Some(cumulative);
        usage.requests += 1;
        if let Some(last) = info.get("last_token_usage") {
            saw_incremental = true;
            for (sum, value) in incremental.iter_mut().zip(token_counts(last)) {
                *sum = sum.saturating_add(value);
            }
        } else {
            all_events_have_incremental = false;
        }
    }
    let counts = if saw_incremental && all_events_have_incremental {
        incremental
    } else {
        last_cumulative.unwrap_or_default()
    };
    if seen {
        usage.input_tokens = counts[0];
        usage.cached_input_tokens = counts[1];
        usage.output_tokens = counts[2];
        usage.reasoning_tokens = counts[3];
        usage.total_tokens = counts[4];
    }
    seen.then_some(usage)
}

fn token_counts(value: &serde_json::Value) -> [u64; 5] {
    [
        token_field(value, "input_tokens"),
        token_field(value, "cached_input_tokens"),
        token_field(value, "output_tokens"),
        token_field(value, "reasoning_output_tokens").max(token_field(value, "reasoning_tokens")),
        token_field(value, "total_tokens"),
    ]
}

fn token_field(value: &serde_json::Value, name: &str) -> u64 {
    value
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// 从一条 `token_count` 事件里取出额度窗口。
///
/// 哪个槽位装哪个窗不固定：Plus 的 `primary` 是 5h 窗、`secondary` 是周窗，Pro 只
/// 有一个周窗的 `primary`。所以槽位名一律丢掉，含义由 `window_minutes` 承载——认
/// 槽位名会把 Pro 的周用量标成 5h。
fn read_rate_limits(payload: &serde_json::Value, event: &serde_json::Value) -> Option<RateLimits> {
    let limits = payload.get("rate_limits")?;
    let windows: Vec<RateWindow> = ["primary", "secondary"]
        .iter()
        .filter_map(|slot| limits.get(slot))
        .filter_map(|window| {
            Some(RateWindow {
                window_minutes: window.get("window_minutes")?.as_u64()?,
                used_percent: window.get("used_percent")?.as_f64()?,
                resets_at: window.get("resets_at").and_then(serde_json::Value::as_i64),
            })
        })
        .collect();
    if windows.is_empty() {
        // 两次刷新之间 OpenAI 会发全 null 的窗口，那不代表"用量为零"。
        return None;
    }
    Some(RateLimits {
        observed_at: event
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        plan: limits
            .get("plan_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        windows,
    })
}

/// rollout 文件名里嵌的会话 id：`rollout-<时间戳>-<uuid>.jsonl`。
///
/// uuid 是最后五个短横分段——时间戳里也有短横，从前面数会切错。
fn rollout_session_id(path: &Path) -> String {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return String::new();
    };
    let Some(rest) = stem.strip_prefix("rollout-") else {
        return String::new();
    };
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() < 5 {
        return String::new();
    }
    parts[parts.len() - 5..].join("-")
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;
