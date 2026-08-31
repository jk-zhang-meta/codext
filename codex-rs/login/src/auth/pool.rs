//! 从自建的账号池服务在线租借 Codex 凭据，替代本机 `auth.json`。
//!
//! 挂在上游的 `ExternalAuth` 扩展点上。`AuthManager::load_auth()` 会优先问这里，
//! 而 `auth()` 每次取凭据都会重新 resolve 一遍，所以换号既不用动任何文件、也不
//! 受「`CODEX_HOME` 启动后不可更改」的限制——同一台机器上的多个进程各租各的号。
//!
//! **每个请求都向池子问一次该用哪个号，本地不做缓存判断。** 稳定性由服务端的调度
//! 算法保证（手上那个还能用就原样还回来，绝不为"别的号更宽裕"而换），不是靠客户端
//! 攥着不放——那样额度跑满了也换不掉。同一趟往返顺带把**用量**带上去。
//!
//! 用量在**一次模型调用结束那一刻**记账（[`record_turn_usage`]），因为只有那一刻
//! 同时知道三件事：这次花了多少 token、用的哪个号、哪个模型。曾经的做法是事后回头
//! 扫 rollout 文件反推归属，那是错的——rollout 不记录每条 `token_count` 是哪个号
//! 跑出来的，只能靠"上次扫到哪儿"做差，而一个没有本地记录的会话会从 0 起算，把它
//! 的**整段历史**一次性记到此刻手上那个号头上。
//!
//! 额度读数同样不由终端上报，理由相同。服务端拿账号自己的 token 直接问 OpenAI。
//!
//! refresh token 永远留在服务端：`CodexAuth::from_external_chatgpt_tokens` 造出
//! 来的凭据本来就不带它，续期由服务端独占。多台机器各自拿着同一个 refresh token
//! 去刷新，只会把彼此的令牌轮换作废。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use codex_http_client::HttpClient;
use codex_utils_path::write_atomically;
use serde::Deserialize;
use serde::Serialize;

use super::access_token::CodexAccessToken;
use super::access_token::classify_codex_access_token;
use super::default_client::create_client;
use super::manager::AuthManager;
use super::manager::CodexAuth;
use super::manager::ExternalAuth;
use super::manager::ExternalAuthFuture;
use super::manager::ExternalAuthRefreshContext;
use super::manager::RefreshTokenError;

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
/// 窗口取 1 秒是有依据的，不是随手定的：真正的模型调用间隔以秒计，所以每个请求
/// 依然拿到一次全新的决策——被合并掉的全是同一个请求内部的重复提问。原来那套是
/// 200 秒，差着两个数量级。
///
/// 401 不走这条路：`refresh()` 永远重新问，手上那份刚被对面拒绝。
const DECISION_COALESCE: Duration = Duration::from_secs(1);

/// 没有请求的时候多久主动找一次池子。
///
/// 派号本身挂在请求路径上，安静的时候不需要它。这个心跳是为了让一个跑完最后一轮
/// 就没有下一个请求的会话也能把用量报上去，顺带把租约续上。
const IDLE_TICK: Duration = Duration::from_secs(20);

/// 一条账目多久没动过就不再上报。
///
/// 上报的是**累计值**、服务端取较大值，所以重复报无害、少报一轮也能补上——只要
/// 这条账还在。留一段时间是为了让一次 `codex exec` 的最后一轮有机会被下一次运行
/// 带上去：那一轮跑完进程就退出了，本次运行里没有下一个请求能捎带它。
const LEDGER_KEEP_SECONDS: u64 = 6 * 3600;

/// 账本最多留多少条，防止一个用了很久的 `CODEX_HOME` 让文件无限长下去。
const MAX_LEDGER_ROWS: usize = 500;

/// v3 表示这份计数是在**每次调用结束那一刻**按 (会话, 号, 模型) 记下的实际用量。
///
/// 必须和 v2 分开：v2 也是这个字段名、这个形状，但那份数字是事后扫 rollout 反推
/// 出来的，归属天生就可能是错的。服务端只收 v3，等于把还没升级的旧二进制报上来的
/// 脏数据挡在外面——宁可暂时没有用量，也不要错的用量。
const USAGE_COUNTER_VERSION: u8 = 3;

/// 本地账本。存**已经发生**的用量，不存任何需要事后推断的东西。
///
/// 落盘是为了跨进程重启：服务端按 (会话, 号, 模型) 取较大值来保证重发幂等，
/// 而一个重启后从 0 重新累计的进程会一直报出比库里更小的数，那些新用量就再也
/// 盖不过去了。文件里没有凭据，只有计数。
const LEDGER_FILE: &str = "pool-usage.json";

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

/// 这个进程装上的池子，退出时要拿它交回名额。见 [`release_on_exit`]。
///
/// 单独存一份而不是从 `AuthManager` 问回来：`set_external_auth` 收下的是
/// `Arc<dyn ExternalAuth>`，退出路径上再把它降回 `PoolAuth` 需要 `Any` 那一套，
/// 而这里要的只是"装了没有"。
static INSTALLED: std::sync::OnceLock<Arc<PoolAuth>> = std::sync::OnceLock::new();

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

/// 本地账本：已发生的用量、手上的号、以及一个"刚被拒"的待报标记。
///
/// 是进程级静态而不是挂在 [`PoolAuth`] 上，因为记账点在 core 里（一次模型调用刚
/// 结束的地方），那里拿不到 `PoolAuth` 实例。和 [`POOL_EXHAUSTED`] 同一个模式。
static LEDGER: std::sync::Mutex<Ledger> = std::sync::Mutex::new(Ledger::new());

#[derive(Default, Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
struct UsageCounters {
    requests: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
}

impl UsageCounters {
    /// 把刚结束的这一次调用加进来。计数只增不减，所以服务端可以安全地取较大值。
    fn add_turn(&mut self, usage: &codex_protocol::protocol::TokenUsage) {
        let add = |slot: &mut u64, value: i64| {
            *slot = slot.saturating_add(u64::try_from(value).unwrap_or(0));
        };
        self.requests = self.requests.saturating_add(1);
        add(&mut self.input_tokens, usage.input_tokens);
        add(&mut self.cached_input_tokens, usage.cached_input_tokens);
        add(&mut self.output_tokens, usage.output_tokens);
        add(&mut self.reasoning_tokens, usage.reasoning_output_tokens);
        add(&mut self.total_tokens, usage.total_tokens);
    }
}

/// 一条账：某个会话在某个号上用某个模型跑出来的累计量。
#[derive(Serialize, Deserialize, Debug)]
struct LedgerRow {
    session_id: String,
    account_key: String,
    #[serde(default)]
    model: String,
    #[serde(flatten)]
    counters: UsageCounters,
    /// 最后一次记账的 unix 秒，只用来裁剪，不上报。
    updated_at: u64,
}

struct Ledger {
    path: Option<PathBuf>,
    /// 此刻手上的号。`None` 表示没租到（退回本机 auth.json），那期间的用量不归池子。
    held: Option<String>,
    /// 手上这个号的邮箱，给 `/status` 用。
    ///
    /// 存在这里而不是现问租约，是因为问租约的那条路（[`PoolAuth::current`]）在
    /// 合并窗之外会**重新派号**——用它来读一眼「现在是哪个号」，会把号读成另一个。
    /// 这个字段只写不问，读它永远不会改变任何东西。
    held_email: Option<String>,
    /// 这条会话**真正的工作目录**，由 core 在每一轮告诉我们。
    ///
    /// 后台原来显示的是 ags 启动时那个 shell 的 `pwd`，而 ags 是在哪儿被敲的和
    /// codex 在哪儿干活是两件事——实测一台机器上四条会话都报 `/root`（HOME），
    /// 而它们各自的 `threads.cwd` 分别在三个不同的项目里。而且那个值只在启动时
    /// 报一次，之后再不更新。
    ///
    /// 只有 core 知道真相，所以由它推过来；不持久化，进程内的事实每轮都会重报。
    cwd: Option<String>,
    /// 手上这个号这一轮失败的原因，等下一次派号时报上去。
    ///
    /// 以前是个 `bool`，只表达得了「配额用尽」一件事，于是终端知道的其它每一种
    /// 失败——403 被停用、402 计费、5xx、连不上——服务端一概看不见。现在带上原因
    /// 本身；**怎么处置由服务端决定**，客户端只负责如实说发生了什么。
    refusal: Option<String>,
    rows: Vec<LedgerRow>,
}

impl Ledger {
    const fn new() -> Self {
        Self {
            path: None,
            held: None,
            held_email: None,
            cwd: None,
            refusal: None,
            rows: Vec::new(),
        }
    }

    fn find(&mut self, session_id: &str, account_key: &str, model: &str) -> &mut LedgerRow {
        if let Some(index) = self.rows.iter().position(|row| {
            row.session_id == session_id && row.account_key == account_key && row.model == model
        }) {
            return &mut self.rows[index];
        }
        self.rows.push(LedgerRow {
            session_id: session_id.to_string(),
            account_key: account_key.to_string(),
            model: model.to_string(),
            counters: UsageCounters::default(),
            updated_at: 0,
        });
        self.rows.last_mut().expect("just pushed")
    }

    /// 丢掉太旧的账，再按条数兜底。旧账早就报上去了，留着只是让文件变长。
    fn prune(&mut self, now: u64) {
        self.rows
            .retain(|row| now.saturating_sub(row.updated_at) <= LEDGER_KEEP_SECONDS);
        if self.rows.len() > MAX_LEDGER_ROWS {
            self.rows
                .sort_by_key(|row| std::cmp::Reverse(row.updated_at));
            self.rows.truncate(MAX_LEDGER_ROWS);
        }
    }

    /// 写不进去不是致命错误：账还在内存里，这次运行照样报得出去，只是重启会丢。
    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        match serde_json::to_string(&self.rows) {
            Ok(body) => {
                if let Err(err) = write_atomically(path, &body) {
                    tracing::debug!("codext: could not persist the usage ledger: {err}");
                }
            }
            Err(err) => tracing::debug!("codext: could not encode the usage ledger: {err}"),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 一次模型调用刚结束：把这次用掉的 token 记到**此刻手上那个号**头上。
///
/// 这是整套用量统计唯一的记账点，也是唯一同时知道 token 数、账号和模型的地方。
/// 上游在 `Session::record_token_usage_info` 里调它——那正是收到
/// `ResponseEvent::Completed` 之后、拿到这一次调用用量的位置。
///
/// 换号只发生在两次调用之间（服务端对能用的号有黏性，只有被拒才换），所以"此刻
/// 手上的号"就是刚才服务这次调用的号。
pub fn record_turn_usage(
    session_id: &str,
    model: &str,
    usage: &codex_protocol::protocol::TokenUsage,
) {
    if session_id.is_empty() {
        return;
    }
    let Ok(mut ledger) = LEDGER.lock() else {
        return;
    };
    // 没租到号的时候跑的用量不是池子的，记上去就是凭空给某个号栽赃。
    let Some(account_key) = ledger.held.clone() else {
        return;
    };
    let now = unix_now();
    let row = ledger.find(session_id, &account_key, model);
    row.counters.add_turn(usage);
    row.updated_at = now;
    ledger.prune(now);
    ledger.persist();
}

/// 手上这个号这一轮被 OpenAI 拒了，记下原因等下一次派号时报上去。
///
/// 这是**拒绝**不是**读数**：它是对方明确的答复，报错了也只会让报告者自己失去手上
/// 这个号。服务端据此立刻把号按下去——否则它只能等后台观测（最快 30 秒）才知道，
/// 而这期间会一次次把同一个已经跑满的号发回给正在重试的会话。
///
/// `reason` 是一个稳定的短标识（见 core 的 `reject_reason`）。**上报什么和服务端
/// 拿它做什么是两件事**：客户端如实说发生了什么，回避多久由服务端按原因自己定。
/// 这条分工是必须的——2026-08-09 把裸 429 也报成「配额用尽」，等于为了一分钟的
/// TPM 拥塞把好号雪藏几小时，池子以分钟级速度被掏空。原因分开之后，那次误报在
/// 结构上不可能再发生：429 报的是 `retry_limit_429`，它根本不在服务端的雪藏名单里。
pub fn report_account_refused(reason: &str) {
    let reason = reason.trim();
    if reason.is_empty() {
        return;
    }
    if let Ok(mut ledger) = LEDGER.lock() {
        // 后来的覆盖先前的：同一次失败会被重试几轮，最后一次的原因最接近现状。
        ledger.refusal = Some(reason.chars().take(64).collect());
    }
}

/// core 每一轮告诉我们这条会话此刻在哪个目录干活。见 [`Ledger::cwd`]。
///
/// 每轮都调，覆盖式写入：会话中途换目录也跟得上，而这正是启动时报一次做不到的。
pub fn set_session_cwd(cwd: &str) {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return;
    }
    if let Ok(mut ledger) = LEDGER.lock() {
        if ledger.cwd.as_deref() != Some(cwd) {
            ledger.cwd = Some(cwd.to_string());
        }
    }
}

fn session_cwd() -> Option<String> {
    LEDGER.lock().ok().and_then(|ledger| ledger.cwd.clone())
}

fn set_held_account(account_key: Option<String>, email: Option<String>) {
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger.held = account_key;
        ledger.held_email = email;
    }
}

/// 此刻派出去的那个号的邮箱，`None` 表示没在用池子。
///
/// `/status` 那一行的邮箱和额度百分比必须说的是同一个号。百分比来自刚回来的那次
/// 响应，所以邮箱也要在同一时刻取——每请求派号意味着「上一次是谁」和「下一次是谁」
/// 通常不是同一个号，晚一步取到的就是另一个号的邮箱。
pub fn held_account_email() -> Option<String> {
    LEDGER
        .lock()
        .ok()
        .and_then(|ledger| ledger.held_email.clone())
}

/// 有没有一条还没送达服务端的拒绝。
fn refusal_reason() -> Option<String> {
    LEDGER.lock().ok().and_then(|ledger| ledger.refusal.clone())
}

/// 拒绝已经报到服务端了，销账。只在派号请求成功之后调。
fn clear_refusal() {
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger.refusal = None;
    }
}

/// 账本里所有还留着的账，按服务端的形状打包。
///
/// **报完不删。** 上报的是累计值、服务端取较大值，所以重复报是空操作；而"发出去了
/// 但回包丢了"和"根本没发出去"在客户端看来一模一样，删掉就等于赌它送到了。裁剪交给
/// [`Ledger::prune`] 按时间做。
fn pending_usage() -> Vec<SessionUsage> {
    let Ok(ledger) = LEDGER.lock() else {
        return Vec::new();
    };
    ledger
        .rows
        .iter()
        .map(|row| SessionUsage {
            session_id: row.session_id.clone(),
            counter_version: USAGE_COUNTER_VERSION,
            account_key: Some(row.account_key.clone()),
            model: (!row.model.is_empty()).then(|| row.model.clone()),
            requests: row.counters.requests,
            input_tokens: row.counters.input_tokens,
            cached_input_tokens: row.counters.cached_input_tokens,
            output_tokens: row.counters.output_tokens,
            reasoning_tokens: row.counters.reasoning_tokens,
            total_tokens: row.counters.total_tokens,
        })
        .collect()
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
    attach_ledger(codex_home);
    let pool = Arc::new(PoolAuth::new(config));
    // 装不上不该让 codext 起不来：留在本地认证上，用户至少还能用自己 `codex login`
    // 登过的号。**但「这一刻租不到号」和「这台机器不允许用池子」必须分开处置。**
    if let Err(err) = manager.set_external_auth(pool.clone()).await {
        // **把枯竭标记清掉。** 启动这一次如果拿到的是 `data: null`，
        // `current()` 已经先 `mark_pool_exhausted()` 了；而清掉它的
        // `mark_pool_serving()` 只在成功派号时调用。
        //
        // 后果是进程级的，而且看起来完全不像这个原因：`pool_is_exhausted()` 会让
        // `ends_the_turn_now` 对一切错误恒为 false、`retry_is_allowed` 恒为 true、
        // `RetryKind::of` 恒返回 `PoolExhausted`。于是一个"模型名写错"的 404 会变成
        // 30 秒一次、永不结束的重试，界面上还写着"去后台加一个账号"。
        //
        // 下面装上 provider 之后，心跳和逐请求两条路都会重新如实标记它。
        mark_pool_serving();
        // 永久性失败（工作负载身份、被管理策略挡掉的登录方式）换多少次都不会变，
        // 装上去只是让每一次取凭据都白跑一趟。保持原样：退回本机认证。
        if matches!(err, RefreshTokenError::Permanent(_)) {
            tracing::warn!("codext: pool auth rejected permanently, keeping local auth: {err}");
            return;
        }
        // 瞬时失败（池子不可达、此刻没号可派、密钥一时被拒）**不能**据此判定这个进程
        // 一辈子都用不上池子——`install_if_configured` 只在启动跑一次，就这一次机会。
        // 无条件把 provider 装上，首次租号推迟到真正要用凭据的时候：`load_auth` 每次
        // 都会重新问它，取不到就退回本机 auth.json，号一回来自动切回池子。
        //
        // 不这么做的话，启动瞬间的一次抖动会把整个进程钉死在本机 auth.json 上，
        // `has_external_auth()` 恒为 false，core 于是判成「没有池子可换」，手上那个号
        // 打满之后一路重试到窗口重置（实测钉了 7.5 小时，其时池子是健康的）。
        tracing::warn!(
            "codext: the pool did not serve a lease at startup ({err}); \
             installing it anyway and leasing on demand"
        );
        manager.set_external_auth_lazy(pool.clone());
    }
    // 登记 + 起心跳。**这两件事必须在瞬时失败的路径上也发生**：`INSTALLED` 决定退出时
    // 交不交回名额，而 `spawn_idle_tick` 正是启动没租到号时把号补上的那条自愈路径——
    // 以前它们都在 `return` 之后，于是最需要自愈的那种情况恰恰一个都没起来。
    let _ = INSTALLED.set(pool.clone());
    spawn_idle_tick(pool);
}

/// 会话结束了：把调度名额交回池子。没配池子、或者池子没装上，就什么都不做。
///
/// **挂在 `cli/src/main.rs` 的 `main` 上**，因为那是整个二进制唯一的汇合点——所有
/// 子命令都从那个闭包里出来。挑挂钩点的教训见 `AuthManager::shared_from_config`
/// 上面那段：按函数级 churn 挑得中"这个函数会不会被改"，挑不中"谁还会调用它"，
/// 所以必须钉在汇合点上。`fn main` 近四个发版一次没动过。
///
/// **它不承诺一定送到。** 进程被 SIGKILL、断网、或者走 `std::process::exit` 的那
/// 几条错误路径时，谁也叫不到这里；服务端的 `CODEX_LEASE_TTL_SECONDS` 仍然是兜底。
/// 这个调用只是把常见的正常退出从"最长一个 TTL 的幽灵持有者"压到接近 0。
///
/// 失败只记调试日志:此刻会话已经在退出了，没有任何人能对这个错误做点什么，而
/// 拖慢或者吓到退出路径是实打实的代价。超时上限是 [`POOL_TIMEOUT`]（3 秒）。
pub async fn release_on_exit() {
    let Some(pool) = INSTALLED.get() else {
        return;
    };
    if let Err(err) = pool.release().await {
        tracing::debug!("codext: could not release the pool lease on exit: {err}");
    }
}

/// 把账本接到 `CODEX_HOME`，并读回上次运行没报完的账。
///
/// 读不出来就从空账本开始：文件损坏、或者上一版留下的旧格式，都不该让 codext 起不来。
/// 代价只是丢掉上次运行的尾巴，而那笔账多半已经报上去过了。
fn attach_ledger(codex_home: &Path) {
    let path = codex_home.join(LEDGER_FILE);
    let rows = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<LedgerRow>>(&raw).ok())
        .unwrap_or_default();
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger.path = Some(path);
        ledger.rows = rows;
        ledger.prune(unix_now());
    }
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
    /// 这次启动点名要的号（账号标识或邮箱）；`None` = 交给服务端调度。
    ///
    /// 每个进程一份，所以同一把密钥的不同会话可以各要各的号。**强首选而不是唯一
    /// 选项**：号冷却或跑满时服务端回落公共池，会话不会停工；号一恢复，下一次
    /// 请求自己排回最前面。
    want_account: Option<String>,
}

/// `CODEX_HOME/pool.json`：`{"base_url": "https://…:844", "key": "…"}`
#[derive(Deserialize)]
struct StoredConfig {
    base_url: String,
    key: String,
    /// 这个 `CODEX_HOME` 的默认账号，选填。给"一个目录固定用一个号"用；临时
    /// 换号仍然走 `CODEXT_POOL_ACCOUNT`，它优先。
    #[serde(default)]
    account: Option<String>,
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
        // 点名的号：环境变量优先，其次 pool.json 里的默认值。ags 起 codex 时注入
        // 的就是这个环境变量——它是**每个进程**一份，所以同一把密钥的不同会话可以
        // 各要各的号而互不影响。
        //
        // 注意：光有它还不够。`device_id` 默认按**工作目录**取（见下），而服务端的
        // 租约表在 `device_id` 上有唯一约束——同一个目录里的两个会话点不同的号，
        // 必须连 `CODEXT_POOL_DEVICE_ID` 一起给，否则两边抢同一条租约行。
        let want_account = env_non_empty("CODEXT_POOL_ACCOUNT").or_else(|| {
            stored
                .as_ref()
                .and_then(|stored| stored.account.as_deref())
                .map(str::trim)
                .filter(|account| !account.is_empty())
                .map(str::to_string)
        });
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            key,
            want_account,
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

struct PoolAuth {
    config: Config,
    client: HttpClient,
    /// 手上这份凭据。**只有两个用途**：告诉服务端"我现在用的是哪个号"，以及在
    /// 池子不可达时兜底。它不做决策短路——每个请求都要重新派一次号，手上这个还
    /// 能不能接着用由服务端判定。
    lease: RwLock<Option<Lease>>,
}

impl PoolAuth {
    fn new(config: Config) -> Self {
        Self {
            config,
            client: create_client(),
            lease: RwLock::new(None),
        }
    }

    /// 派一次号。每次取凭据都会走这里，没有缓存短路。
    ///
    /// `allow_stale` 决定池子联系不上时能不能拿手上那份顶着用。取凭据时可以
    /// （access token 通常还有十几分钟有效期，一次网络抖动不该打断会话）；凭据
    /// 刚被 401 拒绝时不行——那份已经被对面拒了，再用一次只会再失败一次。
    async fn current(&self, allow_stale: bool) -> std::io::Result<CodexAuth> {
        let refusal = refusal_reason();
        let refused = refusal.is_some();
        // 有待报的拒绝时不能走合并窗：那份"刚刚的决定"正是刚被拒的那个号。
        if !refused && let Some(auth) = allow_stale.then(|| self.recent_decision()).flatten() {
            return Ok(auth);
        }
        let held = self.held_account();
        // 401 那条路（`allow_stale == false`）在没有别的原因可报时，报 401 本身。
        let reject = refusal
            .clone()
            .or_else(|| (!allow_stale).then(|| REJECT_UNAUTHORIZED.to_string()));
        let posted = self.post(held.as_deref(), reject.as_deref()).await;
        // **送到了才清标记。** 提前清掉的话，一次网络失败就永久丢掉这条消息，服务端
        // 再也不知道这个号满了，于是一次次把它发回来——正是要修的那个死循环。
        if refused && posted.is_ok() {
            clear_refusal();
        }
        match posted {
            Ok(Some(data)) => {
                let account_key = data.account_key.clone();
                let auth = match Self::validate(&data) {
                    Ok(auth) => auth,
                    Err(err) => {
                        self.abandon_current().await?;
                        return Err(err);
                    }
                };
                mark_pool_serving();
                Ok(self.store(account_key, auth))
            }
            // 「此刻没号可发」是确定的答复，不是抖动：手上那份多半正是服务端刚决定
            // 不再发的那个号（额度到顶、被停用、被冷却），继续骑着只会一路撞墙。所以
            // 这里**不复用**旧租约，报上去让 `AuthManager::load_auth` 退回本机
            // auth.json；下一个请求照样会再问池子一次，号一回来就自动切回去。
            Ok(None) => {
                mark_pool_exhausted();
                self.abandon_current().await?;
                Err(std::io::Error::other("no account available in the pool"))
            }
            // 连不上时可以继续骑手上那份——几十毫秒的抖动不该打断会话。**但有一个
            // 例外：手上这份刚被对面拒过。** 上面那个合并窗已经判了 `refused`，这里
            // 漏判的话就成了 6 秒一圈的死循环：撞 429 → 标记 refused → 重试 → post
            // 失败 → 原样拿回同一个号 → 再撞 429。宁可退回本机 auth.json，那至少是
            // 一个没被拒过的凭据。
            Err(err) if allow_stale && !refused => match self.cached_auth() {
                Some(auth) => {
                    tracing::warn!("codext: pool unreachable, reusing the current lease: {err}");
                    Ok(auth)
                }
                None => {
                    self.abandon_current().await?;
                    Err(err)
                }
            },
            // 派号请求本身失败（连不上、5xx），而且上面那条宽容分支没接住——要么
            // 手上这份刚被拒过，要么这是 401 那条路。
            //
            // **手上还有缓存凭据时不能把 held 抹掉。** 上一层
            // （`manager.rs::load_auth`）在外部凭据解析失败时写着
            // `// Keep serving the last known credential for this call;`——它会继续
            // 用**这个号**发请求。抹掉就造出一个撕裂状态：进程正在用这个号，却对外
            // 声称手上没有号；而 `RetryKind::of` 判 429 是不是账号级、要不要换号、
            // 要不要上报，靠的正是 `held_account_email().is_some()`。于是这条会话的
            // 每个 429 都被归成"网络故障"，进入无限重试：不上报、不换号，而且不会
            // 自己好。2026-08-31 线上那条卡了十六分钟的会话就是这个形状。
            //
            // **只放过这一条路。** `Ok(None)`（服务端明说没号可发）照旧忘掉：那时
            // `mark_pool_exhausted()` 已经让 `RetryKind::of` 走 `PoolExhausted`，不
            // 存在误判；而且服务端本来就不打算再服务这个号，记着它会把退回本地之后
            // 跑掉的用量记到它头上——见
            // `an_empty_pool_does_not_keep_riding_the_old_lease`，那条契约写着理由。
            Err(err) => {
                if self.cached_auth().is_none() {
                    self.abandon_current().await?;
                }
                Err(err)
            }
        }
    }

    async fn abandon_current(&self) -> std::io::Result<()> {
        self.forget_lease();
        Ok(())
    }

    /// 会话要退出了：把名额交回去，顺便把最后一批用量带上。
    ///
    /// 不走 [`PoolAuth::post`]：那个接口的语义是「派一个号给我」，服务端会为它建
    /// 一份新租约——用它来释放等于刚放掉就又占一个。
    ///
    /// 用量一起带走是因为这是这个会话**最后一次开口**。20 秒的 [`IDLE_TICK`] 心跳
    /// 只保证"跑着的时候不会积压太久"，退出前最后那一轮它未必来得及；账本虽然会
    /// 落盘留给下一次运行补报，但"最后一次"常常真的就是最后一次。
    async fn release(&self) -> std::io::Result<()> {
        let url = format!("{}{PATH_PREFIX}/release", self.config.base_url);
        let response = self
            .client
            .post(url.as_str())
            .header(TOKEN_HEADER, self.config.key.as_str())
            .header("Content-Type", "application/json")
            .timeout(POOL_TIMEOUT)
            .json(&ReleaseRequest {
                device_id: &self.config.device_id,
                sessions: pending_usage(),
            })
            .send()
            .await
            .map_err(std::io::Error::other)?;
        let status = response.status();
        if !status.is_success() {
            return Err(std::io::Error::other(format!(
                "pool release failed: {status}"
            )));
        }
        Ok(())
    }

    /// 向池子要一个号。`Ok(None)` 是「此刻没号可发」——那是服务端的正常答复，
    /// 和「联系不上」不是一回事，两者的处置也不一样，见 [`PoolAuth::current`]。
    async fn post(
        &self,
        account_key: Option<&str>,
        reject: Option<&str>,
    ) -> std::io::Result<Option<LeaseData>> {
        let sessions = pending_usage();
        let cwd = session_cwd();
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
                want_account: self.config.want_account.as_deref(),
                cwd: cwd.as_deref(),
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
        let access_token = data.auth_json.tokens.access_token.as_str();
        // 池子两种凭据都会派：ChatGPT 的 OAuth access token（JWT）和 PAT
        // （`at-…`，不透明长期令牌）。**必须按各自的认证模式表示**，因为
        // 它们能访问的端点不同——见 `from_external_personal_access_token`
        // 上面那张实测表。判据用上游自己的前缀约定，不另立一套。
        if matches!(
            classify_codex_access_token(access_token),
            CodexAccessToken::PersonalAccessToken(_)
        ) {
            return Ok(CodexAuth::from_external_personal_access_token(
                access_token,
                Self::user_id_of(&data.account_key),
                account_id,
                data.plan.as_deref(),
            ));
        }
        CodexAuth::from_external_chatgpt_tokens(access_token, account_id, data.plan.as_deref())
    }

    /// `account_key` 是服务端的 `user-…::acct-…`，前半段就是 chatgpt_user_id。
    ///
    /// 取不到就给空串：这个字段只用于显示和遥测，编一个假的比留空更糟。
    fn user_id_of(account_key: &str) -> &str {
        account_key
            .split_once("::")
            .map_or("", |(user_id, _)| user_id)
    }

    fn store(&self, account_key: String, auth: CodexAuth) -> CodexAuth {
        set_held_account(Some(account_key.clone()), auth.get_account_email());
        if let Ok(mut guard) = self.lease.write() {
            *guard = Some(Lease {
                account_key,
                auth: auth.clone(),
                decided_at: Instant::now(),
            });
        }
        auth
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
        set_held_account(None, None);
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

/// 上游的 `ExternalAuthRefreshReason` 只有 `Unauthorized` 一个变体，所以走
/// [`ExternalAuth::refresh`] 报上来的理由只有这一个。
pub const REJECT_UNAUTHORIZED: &str = "unauthorized";

/// 手上这个号被 OpenAI 判了配额用尽。见 [`report_account_refused`]。
///
/// 和 401 分开报：401 是"这份令牌该续期了"，等一轮就好；配额用尽要等整个窗口重置，
/// 两者的回避时长差着几个数量级，混成一个理由必然有一边是错的。
///
/// **这是一个线上契约**：服务端按这个字面量决定要不要把号按下去
/// （`routes/codex_pool.py`）。改字符串等于让所有真配额耗尽都不再被雪藏，所以原因
/// 由 core 分类、字面量在这里只此一份，两边不各写一遍。
pub const REJECT_USAGE_LIMIT: &str = "usage_limit";

/// [`PoolAuth::release`] 的请求体。故意不复用 [`PoolRequest`]：那个结构体的每个
/// 字段都是"派号时告诉服务端的事"，释放一个字段都用不上，共用会让两边的契约互相
/// 牵制——上游给派号加字段时，释放接口不该跟着变。
#[derive(Serialize)]
struct ReleaseRequest<'a> {
    device_id: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<SessionUsage>,
}

#[derive(Serialize)]
struct PoolRequest<'a> {
    device_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reject: Option<&'a str>,
    /// 这次启动点名要的号。见 [`Config::want_account`]。
    #[serde(skip_serializing_if = "Option::is_none")]
    want_account: Option<&'a str>,
    /// 这条会话真正的工作目录。见 [`Ledger::cwd`]。
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
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
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;
