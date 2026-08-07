//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use chrono::Utc;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

/// codext: 采样路径上仍然终结这一轮的全部错误。
///
/// 注意这里**没有**"重试没用所以我们放弃"这一类——那个判断已经不归我们做了。
/// 剩下的四个各有各的理由：
///
/// - `TurnAborted` / `Interrupted` 就是用户按下的暂停。重试它们等于让 Esc 失灵，
///   而"用户随时可以自己叫停"正是无限重试能成立的前提——把这个出口堵上，剩下的
///   设计全部变成挂死。
/// - `ContextWindowExceeded` 由上面那个专门的分支处理：它要先记满 token 数才能
///   触发压缩。同一个超长请求重试一万次还是装不下，而用户在一轮**之内**没有任何
///   办法把它变短。
/// - `SessionBudgetExceeded` 是用户自己设的开销上限。绕过它重试等于把这个设置
///   悄悄作废。
/// - `CyberPolicy` 是服务端的安全策略拒绝。它跟别的错误不是一回事：那些是**故障**，
///   这个是**答复**。把一个安全判定塞进无限循环里反复问，既问不出别的结果，性质上
///   也不该由客户端自动去做。
pub(crate) fn ends_the_turn(err: &CodexErr) -> bool {
    matches!(
        err.details(),
        CodexErrorDetails::TurnAborted
            | CodexErrorDetails::Interrupted
            | CodexErrorDetails::ContextWindowExceeded
            | CodexErrorDetails::SessionBudgetExceeded
            | CodexErrorDetails::CyberPolicy { .. }
    )
}

/// codext: 这一轮到此为止吗——采样循环真正问的那个问题。
///
/// [`ends_the_turn`] 那五个永远成立。这里多一条：宿主自己设过 `stream_max_retries`
/// 时，"设过 = 一字不差回到上游"这条承诺必须**包括上游那句 `!err.is_retryable()`
/// 的就地退出**。只在 [`retry_is_allowed`] 里认这个开关是不够的——不可重试的错误
/// 照样会先落进重试循环，在上限之内白打一趟，而那一趟对调用方是可见的。
///
/// 0.147.0 的 guardian 复核就是被这一趟打中的：它给自己的子会话设了
/// `stream_max_retries = Some(1)`，然后**自己**按 `max_attempts` 重试、把底层错误
/// 写进拒绝理由。我们多打的那一趟既吃掉了它的 attempt 计数，也吃掉了它 mock 序列
/// 里的下一个响应，于是"重试一次就通过"变成"一次就通过"，"把 responses API 的错误
/// 报给用户"变成报了一个对不上的错误。
///
/// 这不是给 guardian 开的特例：任何一个把自己的重试策略写在外面、并且用这个配置项
/// 表达过"我自己管"的调用方，都该拿到上游那套行为。
///
/// 账号级失败和池子枯竭仍然不受这个开关约束——那两件事是这套东西存在的理由。
///
/// 连带的一条教训：`is_retryable()` 是**全仓共用的分类**，guardian、远端压缩、传输
/// 回退、app-server 都读它。我们一度把 `ServerOverloaded` 在那里翻成"可重试"来表达
/// "服务器忙就等着，别掐会话"——那是把**我们的策略**写进了**别人的分类**，0.147.0
/// 的 guardian 一上来就被它绊倒。策略留在这个模块里就够了：无限重试那条路根本不问
/// `is_retryable()`，它问的是 [`ends_the_turn`]。
pub(crate) fn ends_the_turn_now(err: &CodexErr, turn_context: &TurnContext) -> bool {
    if ends_the_turn(err) {
        return true;
    }
    honors_ceiling(turn_context)
        && !err.is_retryable()
        && !is_account_scoped(err)
        && !codex_login::pool_is_exhausted()
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retries: &mut u64,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
) -> Result<(), CodexErr> {
    // codext: `err.is_retryable()` 这一项是新加的，补的是上游一个隐含前提：以前
    // `turn.rs` 会先把不可重试的错误挡在外面，所以这里看到的必然是传输类故障。现在
    // 它们会走到这儿，而"模型不支持图片输入"这种拒绝换条通道重发还是同样被拒——白
    // 打一趟不说，本该直接报给用户的错误会变成一次无声的重试。
    //
    // `ServerOverloaded` 要单列：上游把它归为不可重试（我们不再去改那个共用分类，
    // 见 `ends_the_turn_now`），但对"这条通道换一条试试"来说它是值得试的——一个
    // 只在 websocket 上挤爆的后端，回退到 HTTPS 确实可能通。
    if (err.is_retryable() || matches!(err.details(), CodexErrorDetails::ServerOverloaded))
        && *retries >= max_retries
        && client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            &turn_context.model_info,
        )
    {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        *retries = 0;
        return Ok(());
    }

    // codext: 只有租来的凭据和空池子无条件不受上限约束——那两件事是这套东西存在的
    // 理由，配置改不掉。其余的看用户有没有自己设过 `stream_max_retries`。
    let unconditional = is_account_scoped(&err) || codex_login::pool_is_exhausted();
    let kind = (unconditional || !honors_ceiling(turn_context))
        .then(|| RetryKind::of(&err, turn_context, sess));

    if retry_is_allowed(&err, *retries, max_retries, request, kind.is_some()) {
        *retries = retries.saturating_add(1);
        let retry_count = *retries;
        let base = err
            .retry_delay()
            .unwrap_or_else(|| backoff(retry_count.min(max_retries.max(1))));

        let Some(kind) = kind else {
            // 用户自己设了上限，那就一字不差地按上游走——退避曲线、日志、措辞全都是。
            log_retry(request, turn_context, &err, retry_count, max_retries, base);
            // In release builds, hide the first websocket retry notification to reduce noisy
            // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
            if retry_count > 1
                || cfg!(debug_assertions)
                || !sess.services.model_client.responses_websocket_enabled()
            {
                sess.notify_stream_error(
                    turn_context,
                    format!("Reconnecting... {retry_count}/{max_retries}"),
                    err,
                )
                .await;
            }
            tokio::time::sleep(base).await;
            return Ok(());
        };

        let delay = kind.delay(base, retry_count, max_retries);
        kind.log(request, turn_context, &err, retry_count, max_retries, delay);

        if kind.should_report(retry_count, sess) {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            sess.notify_stream_error(turn_context, kind.message(), err)
                .await;
        }
        tokio::time::sleep(delay).await;
        return Ok(());
    }

    Err(err)
}

/// codext: 用户有没有自己给重试次数封过顶。
///
/// `stream_max_retries` 现在是**退出开关**：没设过就是"永不放弃"，设过就完全回到
/// 上游行为。把开关做成一个本来就存在的配置项，而不是新造一个 codext 专用的，是
/// 因为它表达的正是这个意思——而且"我不想要这个行为"就变成一句配置的事，不必去
/// 改代码或者换回官方 codex。
fn honors_ceiling(turn_context: &TurnContext) -> bool {
    turn_context.provider.info().stream_max_retries.is_some()
}

/// codext: 采样路径上，除了 [`ends_the_turn`] 那四个，一律重试，没有次数上限。
///
/// 这是个产品决定，不是技术判断：断开一个跑到一半的会话，代价永远高于多等一会儿。
/// 能修的错误（模型名写错、余额欠费、代理配错）用户看到提示以后可以去修，修好了
/// 下一次重试就通了；修不了的，用户按 Esc。两条路都比我们替他做决定强。
///
/// 远端压缩是唯一的例外，理由见分支上的注释。
fn retry_is_allowed(
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    request: ResponsesStreamRequest,
    unbounded: bool,
) -> bool {
    match request {
        ResponsesStreamRequest::Sampling if unbounded => !ends_the_turn(err),
        // 远端压缩不是会话本身，是一轮**里面**的一步，而且它失败有本地压缩兜底。
        // 在这里无限等会把整轮挂死，还顺手挡掉那个兜底——比报错更糟。所以它永远
        // 保留上限，跟采样那边设没设开关无关。用户自己封了顶的采样也走这一支。
        _ => {
            is_account_scoped(err)
                || codex_login::pool_is_exhausted()
                || matches!(err.details(), CodexErrorDetails::ServerOverloaded)
                || retries < max_retries
        }
    }
}

/// 这一次重试在等什么。
///
/// 分类**不决定要不要重试**（采样路径上答案永远是要），只决定等多久、日志怎么写、
/// 以及那句只说一次的话怎么说。把它收成一个枚举，是因为这三张表必须对齐：一个
/// 三十秒轮询一次的等待配上"正在重连 1/5"的措辞，比不说话还糟。
#[derive(Clone, Copy)]
enum RetryKind {
    /// 池子一个号都发不出来。等的是有人去后台加号。
    PoolExhausted,
    /// 手上这个号额度耗尽，而且有池子可以换。等的是下一次取凭据换个号。
    SwapAccount,
    /// 额度耗尽，但没有池子可换。等的是这个号自己的窗口重置。
    QuotaWindow { resets_in: Option<Duration> },
    /// 模型满载或服务端过载。等的是对面腾出容量。
    Capacity,
    /// 连接失败、超时、5xx。等的是网络或者对面自己恢复。
    Transient,
    /// 同样的请求会以同样的方式失败：模型名写错的 404、代理返回的 HTML、欠费。
    /// 照样重试——但话要说成"它不会自己好"，否则用户不知道该去修什么。
    Stuck,
}

impl RetryKind {
    fn of(err: &CodexErr, turn_context: &TurnContext, sess: &Session) -> Self {
        // 池子枯竭优先于错误本身的分类：这时候拿到的任何认证类失败，根因都是
        // "没号可发"，按它的表面症状去说只会把人引到错的地方。
        if codex_login::pool_is_exhausted() {
            return Self::PoolExhausted;
        }
        if is_account_scoped(err) {
            return if leases_credentials(turn_context) {
                // codext: 对面刚明确拒了手上这个号。报上去，否则服务端要等后台观测
                // （最快 30 秒）才知道，这期间会把同一个跑满的号一次次发回来。
                codex_login::report_account_refused();
                Self::SwapAccount
            } else {
                Self::QuotaWindow {
                    resets_in: quota_resets_in(err),
                }
            };
        }
        let trust_status_codes = !sess.services.model_client.responses_websocket_enabled();
        match err.details() {
            CodexErrorDetails::ServerOverloaded => Self::Capacity,
            _ if will_not_fix_itself(err, trust_status_codes) => Self::Stuck,
            _ => Self::Transient,
        }
    }

    fn delay(self, base: Duration, retry_count: u64, max_retries: u64) -> Duration {
        match self {
            // 在等**人**去后台加号，不是等网络恢复，问得密没有意义；但也不能太稀，
            // 加完号总得让会话尽快接上。服务端给的任何延迟在这里都不相关。
            Self::PoolExhausted => POOL_EXHAUSTED_RETRY_DELAY,
            Self::SwapAccount => base.clamp(ACCOUNT_SWAP_MIN_DELAY, MAX_RETRY_DELAY),
            // 窗口重置是小时到天的尺度。按剩余时间睡，但封顶——`resets_at` 可能偏，
            // 也可能有人在别处把额度让出来，睡满六天就再也醒不过来了。
            Self::QuotaWindow { resets_in } => resets_in
                .unwrap_or(QUOTA_WINDOW_MAX_DELAY)
                .clamp(QUOTA_WINDOW_MIN_DELAY, QUOTA_WINDOW_MAX_DELAY),
            // 不会自己好的错误没必要按毫秒级退避去刷——用户得有时间读那条提示、
            // 去改配置。直接进慢档。
            Self::Stuck => base.clamp(UNBOUNDED_RETRY_MIN_DELAY, MAX_RETRY_DELAY),
            // 前几次保持上游那条曲线（200ms 起步能很快救回一次抖动），超出上限之后
            // 才加下限——否则 `stream_max_retries = 0` 会变成每秒五次的热循环。
            Self::Capacity | Self::Transient => {
                if retry_count > max_retries {
                    base.clamp(UNBOUNDED_RETRY_MIN_DELAY, MAX_RETRY_DELAY)
                } else {
                    base.min(MAX_RETRY_DELAY)
                }
            }
        }
    }

    /// 无限重试的话**只说一次**：说清楚出了什么事、说明它会一直重试、说明怎么停下，
    /// 然后闭嘴。每隔几十秒把同一句话再刷一遍等于把这条提示自己淹掉，而一个不断增长
    /// 的 attempt 计数看起来像是坏了，不像在等。
    fn should_report(self, retry_count: u64, sess: &Session) -> bool {
        match self {
            // 边沿触发的：多个会话同时撞上枯竭只报一条，而恢复供号会把标记清掉，
            // 下一次枯竭照样报。
            Self::PoolExhausted => codex_login::take_pool_exhaustion_notice(),
            // In release builds, hide the first websocket retry notification to reduce noisy
            // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
            Self::Transient => {
                let hide_first = !cfg!(debug_assertions)
                    && sess.services.model_client.responses_websocket_enabled();
                retry_count == if hide_first { 2 } else { 1 }
            }
            _ => retry_count == 1,
        }
    }

    fn message(self) -> String {
        match self {
            // 这一条是给管理员看的：调度只能摊，变不出配额。
            Self::PoolExhausted => "No account is available in the account pool. The session will \
                                    not be interrupted — it keeps retrying indefinitely until an \
                                    account is added or a quota window resets. Press Esc to stop."
                .to_string(),
            Self::SwapAccount => "This account is out of quota. Leasing another account and \
                                  retrying indefinitely until the pool can serve this session. \
                                  Press Esc to stop."
                .to_string(),
            Self::QuotaWindow { resets_in } => {
                let when = match resets_in {
                    Some(resets_in) => format!(" in about {}", humanize(resets_in)),
                    None => String::new(),
                };
                format!(
                    "This account is out of quota and no account pool is configured, so there is \
                     nothing to switch to. Retrying indefinitely until the window resets{when}. \
                     Press Esc to stop."
                )
            }
            Self::Capacity => "Selected model is at capacity. Retrying indefinitely until it \
                               becomes available. Press Esc to stop."
                .to_string(),
            Self::Transient => "Connection to the model failed. Retrying indefinitely until it \
                                recovers. Press Esc to stop."
                .to_string(),
            Self::Stuck => "This request fails the same way every time and will not recover on \
                            its own. Retrying indefinitely so you can fix the cause without \
                            losing the session; press Esc to stop."
                .to_string(),
        }
    }

    fn log(
        self,
        request: ResponsesStreamRequest,
        turn_context: &TurnContext,
        err: &CodexErr,
        retries: u64,
        max_retries: u64,
        delay: Duration,
    ) {
        let waiting_for = match self {
            Self::PoolExhausted => "account pool has nothing to lease",
            Self::SwapAccount => "account is out of quota - leasing another one",
            Self::QuotaWindow { .. } => "account is out of quota and there is no pool to swap to",
            Self::Capacity => "selected model is at capacity",
            Self::Stuck => "request will not recover on its own",
            // 瞬时故障沿用上游那条日志：它带着 retries/max_retries，是排查抖动时最有用的形状。
            Self::Transient => {
                return log_retry(request, turn_context, err, retries, max_retries, delay);
            }
        };
        warn!(
            turn_id = %turn_context.sub_id,
            retries,
            sampling_error = %err,
            "{waiting_for} - retrying in {delay:?}"
        );
    }
}

/// 账号级失败：换一个还有余量的号就能继续，在同一个号上等没有意义。
///
/// **只有 `UsageLimitReached`。** `QuotaExceeded` 看着像同类，其实是计费状态
/// （"Quota exceeded. Check your plan and billing details."）：它不随额度窗口重置
/// 恢复，也不携带额度读数，换号救不了它——它属于 [`RetryKind::Stuck`]，重试照旧，
/// 但要告诉用户去把账单修好。
pub(crate) fn is_account_scoped(err: &CodexErr) -> bool {
    matches!(err.details(), CodexErrorDetails::UsageLimitReached(_))
}

/// 同一个请求会以完全相同的方式失败，除非**外面**有人改点什么。
///
/// 只影响措辞，不影响要不要重试。分出来是因为这两句话对用户的意义完全不同：
/// "网断了，在等它回来" 让人去泡杯茶，"这个错不会自己好" 让人去看配置。
fn will_not_fix_itself(err: &CodexErr, trust_status_codes: bool) -> bool {
    match err.details() {
        CodexErrorDetails::Json(_)
        | CodexErrorDetails::TokioJoin(_)
        | CodexErrorDetails::InternalAgentDied
        | CodexErrorDetails::InvalidRequest(_)
        | CodexErrorDetails::InvalidImageRequest()
        | CodexErrorDetails::UnsupportedOperation(_)
        | CodexErrorDetails::QuotaExceeded
        | CodexErrorDetails::UsageNotIncluded
        | CodexErrorDetails::CyberPolicy { .. }
        | CodexErrorDetails::AgentLimitReached { .. }
        | CodexErrorDetails::ThreadNotFound(_)
        | CodexErrorDetails::SessionConfiguredNotFirstEvent
        | CodexErrorDetails::EnvVar(_)
        | CodexErrorDetails::Fatal(_)
        | CodexErrorDetails::Spawn
        | CodexErrorDetails::Sandbox(_)
        | CodexErrorDetails::LandlockSandboxExecutableNotProvided => true,
        // 4xx 是"你发的请求有问题"，重试同一个请求得到同一个答案。408/409/425/429
        // 例外：它们说的是"现在不行"，不是"这样不行"。
        //
        // `trust_status_codes` 是这条规则的前提：一次 WebSocket 升级被拒同样是 4xx
        // （挂在没有那条路由的服务上就是 404），但那说的是"这条通道走不通"，回退到
        // HTTPS 自己会解决，把它说成"去改配置"会把人指到完全错误的地方。所以只有
        // WebSocket 根本不参与时，状态码才能拿来下这个结论。
        _ => trust_status_codes
            && matches!(err.details(), CodexErrorDetails::UnexpectedStatus(_))
            && err.http_status_code_value().is_some_and(|status| {
                (400..500).contains(&status) && !RETRYABLE_4XX.contains(&status)
            }),
    }
}

const RETRYABLE_4XX: [u16; 4] = [408, 409, 425, 429];

fn leases_credentials(turn_context: &TurnContext) -> bool {
    turn_context
        .provider
        .auth_manager()
        .is_some_and(|manager| manager.has_external_auth())
}

fn quota_resets_in(err: &CodexErr) -> Option<Duration> {
    let CodexErrorDetails::UsageLimitReached(err) = err.details() else {
        return None;
    };
    (err.resets_at? - Utc::now()).to_std().ok()
}

fn humanize(delay: Duration) -> String {
    let minutes = delay.as_secs() / 60;
    match minutes {
        0 => "a minute".to_string(),
        1..=90 => format!("{minutes} minutes"),
        _ => format!("{} hours", minutes.div_ceil(60)),
    }
}

/// 账号级重试至少等这么久。
///
/// 换号不是这里主动做的：重试会重新取一次凭据，池子每次取凭据都重新派号，而它
/// 判断"手上这个号还能不能用"靠的是终端扫 rollout 报上去的额度读数——那个扫描
/// 节流到 5 秒一次（`codex-login` 的 `pool.rs::USAGE_SCAN_MIN_INTERVAL`）。等不够
/// 5 秒就重试，报上去的是空的，池子拿着旧读数把**同一个号**再派回来，于是撞第二次。
/// 改动那个常量时这里必须跟着改。
const ACCOUNT_SWAP_MIN_DELAY: Duration = Duration::from_secs(6);

/// 无限重试时的延迟下限。
///
/// `backoff(retry_count.min(max_retries.max(1)))` 在 `stream_max_retries = 0` 时
/// 恒定为 `backoff(1)` ≈ 200 毫秒。次数上限没了之后，那就是每秒五次的热循环。
const UNBOUNDED_RETRY_MIN_DELAY: Duration = Duration::from_secs(1);

/// 池子一个号都发不出来时，隔多久回去问一次。
const POOL_EXHAUSTED_RETRY_DELAY: Duration = Duration::from_secs(30);

/// 没有池子、只能等自己额度窗口重置时的轮询区间。
///
/// 上限存在的理由是不能一觉睡到窗口重置：`resets_at` 只是服务端的估计，而且额度
/// 也可能在那之前就被别处让出来。下限存在的理由是这时候问快了毫无意义，只会拿
/// 一串 429 去撞同一个号。
const QUOTA_WINDOW_MIN_DELAY: Duration = Duration::from_secs(60);
const QUOTA_WINDOW_MAX_DELAY: Duration = Duration::from_secs(600);

/// 任何一次重试最多等这么久。
///
/// 对**所有**错误生效（窗口等待除外，那个有自己的一套）：`retry_delay` 是从服务端
/// 消息里正则抠出来的任意秒数，没有上界——一个返回 "try again in 86400s" 的网关能
/// 把会话冻住一天。
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                sampling_error = %err,
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
