//! 从 PersonalWeb 账号池在线租借 Codex 凭据，替代本机 `auth.json`。
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

/// 归属表最多留多少条。够覆盖最近几天，又不至于无限长下去。
const MAX_ATTRIBUTIONS: usize = 500;

/// 我们自己那份配置在 `CODEX_HOME` 下的文件名。
///
/// 刻意不塞进上游的 `config.toml`：那要改 config crate 的类型定义，每次合并上游
/// 都得重新对一遍。单独一个文件，上游永远不会碰。
const CONFIG_FILE: &str = "pool.json";

/// 配了池子就接管凭据来源；没配就什么都不做，codext 退回上游原本的 auth.json。
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
    /// 环境变量在前，是因为 ags 启动 codext 时就是这么把地址和密钥递进来的；
    /// 文件是给「不经 ags、直接跑 codext」准备的。两处都没有就返回 None。
    fn load(codex_home: &Path) -> Option<Self> {
        let stored = std::fs::read_to_string(codex_home.join(CONFIG_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str::<StoredConfig>(&raw).ok())
            .filter(|stored| !stored.base_url.trim().is_empty() && !stored.key.trim().is_empty());

        let base_url = env_non_empty("CODEXT_POOL_URL")
            .or_else(|| stored.as_ref().map(|stored| stored.base_url.trim().to_string()))?;
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
            }),
        }
    }

    /// 派一次号。每次取凭据都会走这里，没有缓存短路。
    ///
    /// `allow_stale` 决定池子联系不上时能不能拿手上那份顶着用。取凭据时可以
    /// （access token 通常还有十几分钟有效期，一次网络抖动不该打断会话）；凭据
    /// 刚被 401 拒绝时不行——那份已经被对面拒了，再用一次只会再失败一次。
    async fn current(&self, allow_stale: bool) -> std::io::Result<CodexAuth> {
        if allow_stale
            && let Some(auth) = self.recent_decision()
        {
            return Ok(auth);
        }
        let held = self.held_account();
        // 只有手上有号才带用量：新出现的会话得记在某个号头上，没有号就没有归属。
        let sessions = match held.as_deref() {
            Some(account_key) => self.scan_usage(account_key).await,
            None => Vec::new(),
        };
        let reject = (!allow_stale).then_some(REJECT_UNAUTHORIZED);
        match self.post(held.as_deref(), reject, sessions).await {
            Ok(data) => self.store(data),
            Err(err) if allow_stale => match self.cached_auth() {
                Some(auth) => {
                    tracing::warn!("codext: pool unreachable, reusing the current lease: {err}");
                    Ok(auth)
                }
                None => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    /// 扫一遍最近的 rollout，取出用量和额度读数，并记下每个会话归谁。
    ///
    /// 节流到 [`USAGE_SCAN_MIN_INTERVAL`]：派号是每个请求一次，重读几 MB 的会话
    /// 文件没必要跟着这么密。服务端按 (session_id, account_key) 去重、计数取较大
    /// 值，所以少报一轮或重复报都无害。
    async fn scan_usage(&self, held: &str) -> Vec<SessionUsage> {
        let started = {
            let Ok(mut guard) = self.scan.write() else {
                return Vec::new();
            };
            let now = Instant::now();
            if now < guard.next {
                return Vec::new();
            }
            guard.next = now + USAGE_SCAN_MIN_INTERVAL;
            guard.started
        };
        let home = self.codex_home.clone();
        let held = held.to_string();
        // 扫目录和读文件都是阻塞 IO，不能直接压在异步线程上。
        tokio::task::spawn_blocking(move || collect_sessions(&home, &held, started))
            .await
            .unwrap_or_default()
    }

    async fn post(
        &self,
        account_key: Option<&str>,
        reject: Option<&str>,
        sessions: Vec<SessionUsage>,
    ) -> std::io::Result<LeaseData> {
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
        // data 为 null 表示「池子里此刻没号可发」。对服务端那是正常状态，对这里
        // 依然是拿不到凭据。
        envelope
            .data
            .ok_or_else(|| std::io::Error::other("no account available in the pool"))
    }

    fn store(&self, data: LeaseData) -> std::io::Result<CodexAuth> {
        let account_id = data.auth_json.tokens.account_id.as_deref().ok_or_else(|| {
            std::io::Error::other("pool returned credentials without a chatgpt account id")
        })?;
        let auth = CodexAuth::from_external_chatgpt_tokens(
            &data.auth_json.tokens.access_token,
            account_id,
            data.plan.as_deref(),
        )?;
        if let Ok(mut guard) = self.lease.write() {
            *guard = Some(Lease {
                account_key: data.account_key,
                auth: auth.clone(),
                decided_at: Instant::now(),
            });
        }
        Ok(auth)
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

#[derive(Serialize, Debug, PartialEq)]
struct RateLimits {
    observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    windows: Vec<RateWindow>,
}

#[derive(Serialize, Debug, PartialEq)]
struct RateWindow {
    window_minutes: u64,
    used_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<i64>,
}

/// 最近动过的会话，附上各自归属的账号。
///
/// 归属三条规则，顺序不能变：
///
/// 1. 有记录的沿用记录——一个会话可能是上一次运行、在另一个号上跑的
/// 2. 没记录、但本次运行动过 → 归手上这个号，并记下来
/// 3. 没记录、本次运行也没动过 → **整条丢掉**
///
/// 第 3 条不能省。同一个 `CODEX_HOME` 里躺着装池子之前的历史、以及用户自己
/// `codex login` 跑的会话；认领它们等于把一整份用量凭空记到当前这个号头上。
/// 用量唯一键是 (session_id, account_key)，那不是覆盖而是多出一整行——真机第一
/// 次跑就撞上了，一个池子建立之前的会话被记了 24601 个 token。
fn collect_sessions(codex_home: &Path, held: &str, started: SystemTime) -> Vec<SessionUsage> {
    let mut owners = load_attribution(codex_home);
    let mut seen = Vec::new();
    let mut sessions = Vec::new();
    for (modified, path) in recent_rollouts(codex_home) {
        let session_id = rollout_session_id(&path);
        if session_id.is_empty() {
            continue;
        }
        let owner = match owners.get(&session_id) {
            Some(owner) => owner.clone(),
            None if modified >= started => {
                owners.insert(session_id.clone(), held.to_string());
                held.to_string()
            }
            None => continue,
        };
        seen.push(session_id);
        // 归属**按文件名认，不等有用量才认**。一次运行里最早那次扫描通常发生在
        // 第一个响应之前，那时 rollout 还没有 `token_count`；要是那时不落归属，
        // 这个会话就永远没有主人，下一次运行也不敢认领它——于是每一轮的最后一次
        // 响应都会丢，而只跑一轮的 `codex exec` 等于什么都报不出来。
        let Some(mut usage) = read_rollout(&path) else {
            continue;
        };
        usage.account_key = Some(owner);
        sessions.push(usage);
    }
    save_attribution(codex_home, &owners, &seen);
    sessions
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

fn load_attribution(codex_home: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(attribution_path(codex_home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 存回归属表，只留最近扫到的那些。
///
/// 按「这一批扫到了什么」裁剪而不是按条数裁：扫的范围本来就是最近这段时间，超出
/// 那个范围的会话再也不会被上报，留着只会让文件无限长下去。
fn save_attribution(
    codex_home: &Path,
    owners: &std::collections::HashMap<String, String>,
    seen: &[String],
) {
    let kept: std::collections::HashMap<&str, &str> = seen
        .iter()
        .take(MAX_ATTRIBUTIONS)
        .filter_map(|session_id| {
            owners
                .get(session_id)
                .map(|owner| (session_id.as_str(), owner.as_str()))
        })
        .collect();
    let Ok(body) = serde_json::to_string(&kept) else {
        return;
    };
    // 写不进去不影响这次上报，下次再试。
    let _ = std::fs::write(attribution_path(codex_home), body);
}

fn collect_rollouts(dir: &Path, since: SystemTime, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
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
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
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
/// （本会话累计）也有 `last_token_usage`（这一次）。总量取**最后一条**，不是把每
/// 条加起来——累计值相加会把一个会话的消耗乘以它的回合数，长会话能报出几亿个它
/// 根本没用过的 token。请求数是唯一需要累加的。
fn read_rollout(path: &Path) -> Option<SessionUsage> {
    let session_id = rollout_session_id(path);
    if session_id.is_empty() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut usage = SessionUsage {
        session_id,
        ..SessionUsage::default()
    };
    let mut seen = false;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("token_count") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = event.get("payload").unwrap_or(&event);
        if payload.get("type").and_then(serde_json::Value::as_str) != Some("token_count") {
            continue;
        }
        let Some(info) = payload.get("info") else {
            continue;
        };
        seen = true;
        usage.requests += 1;
        if let Some(model) = payload
            .get("model")
            .or_else(|| info.get("model"))
            .and_then(serde_json::Value::as_str)
        {
            usage.model = Some(model.to_string());
        }
        // 留最后一条读数：最新的那次调用才反映账号此刻的额度。
        if let Some(limits) = read_rate_limits(payload, &event) {
            usage.rate_limits = Some(limits);
        }
        // 优先用累计块；退回旧 rollout 的扁平结构，免得升级丢掉历史。
        let totals = info.get("total_token_usage").unwrap_or(info);
        usage.input_tokens = token_field(totals, "input_tokens");
        usage.cached_input_tokens = token_field(totals, "cached_input_tokens");
        usage.output_tokens = token_field(totals, "output_tokens");
        usage.reasoning_tokens =
            token_field(totals, "reasoning_output_tokens").max(token_field(totals, "reasoning_tokens"));
        usage.total_tokens = token_field(totals, "total_tokens");
    }
    seen.then_some(usage)
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
