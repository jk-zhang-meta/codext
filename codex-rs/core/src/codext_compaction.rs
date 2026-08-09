//! codext：压缩失败之后的恢复。
//!
//! 这个文件整个是 codext 的，上游没有对应物。放在这里而不是写进
//! `session/turn.rs`，是因为**上游足迹越小，合并越便宜**：`turn.rs` 是上游改动最
//! 频繁的文件之一，把九十行逻辑塞进去意味着每次合并都要重新解释一遍它们为什么在
//! 那儿。现在 `turn.rs` 里只剩几行调用。
//!
//! ## 为什么需要它
//!
//! 上游的自动压缩是**按 provider 能力选路**的：支持远端就走远端，只有
//! `RemoteCompactionSupport::Unsupported` 才走本地——也就是说本地压缩从来不是远端
//! 失败时的兜底。而 `responses_retry` 里给远端压缩保留重试上限的理由，写的正是
//! "它失败有本地压缩兜底"。那个前提不成立。
//!
//! 后果在有账号池时格外难受：自动压缩之所以发生，是因为上下文已经满了，压不动就
//! 真的走不下去。用户看到一句 "Error running remote compact task: You've hit your
//! usage limit…"，而手上明明还有二十几个号可用。
//!
//! ## 两件事，缺一不可
//!
//! 1. **先把"这个号不行"报给池子。** 远端压缩这条路**根本到不了**
//!    `handle_retryable_response_stream_error`——`compact_remote_v2.rs` 里有一句
//!    `Err(err) if !err.is_retryable() => return Err(err)` 挡在前面，而
//!    `is_retryable()` 对 `UsageLimitReached` / `QuotaExceeded` / `UsageNotIncluded`
//!    全是 false。于是 `RetryKind::of` 里那个唯一会调 `report_account_refused()` 的
//!    分支，在这条路上从来没被执行过。
//! 2. **再退回本地压缩。** 不做第 1 步，本地这一趟会重新取凭据、而池子因为不知情
//!    又把同一个已经耗尽的号发回来，五次重试全部撞在同一堵墙上——兜底等于没有。

use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;

use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tracing::warn;

use crate::compact::InitialContextInjection;
use crate::compact::run_inline_auto_compact_task;
use codex_protocol::error::Result as CodexResult;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tasks::emit_compact_metric;

/// 复制一份注入设置，供本地兜底那一趟使用。
///
/// 手写而不是给上游的 `InitialContextInjection` 加 `#[derive(Clone)]`：两个变体装
/// 的都是 `Arc`，克隆只是加引用计数；而那个 enum 在上游的 `compact.rs` 里，为了省
/// 这几行去动它，等于给每一次上游合并留一个新的冲突点。
pub(crate) fn clone_injection(injection: &InitialContextInjection) -> InitialContextInjection {
    match injection {
        InitialContextInjection::BeforeLastUserMessage {
            world_state,
            step_context,
        } => InitialContextInjection::BeforeLastUserMessage {
            world_state: Arc::clone(world_state),
            step_context: Arc::clone(step_context),
        },
        InitialContextInjection::DoNotInject => InitialContextInjection::DoNotInject,
    }
}

/// 远端压缩失败之后：报账号、说一声、退回本地压缩。
///
/// 用户自己叫停不在此列——`TurnAborted` / `Interrupted` 就是 Esc，退回本地等于让
/// Esc 失灵，而"随时可以自己叫停"正是整套无限重试能成立的前提。
pub(crate) async fn recover_from_remote_failure(
    sess: &Arc<Session>,
    turn_context: Arc<TurnContext>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
    err: CodexErr,
) -> CodexResult<()> {
    if matches!(
        err.details(),
        CodexErrorDetails::TurnAborted | CodexErrorDetails::Interrupted
    ) {
        return Err(err);
    }

    if crate::responses_retry::is_account_scoped(&err) {
        codex_login::report_account_refused();
    }

    warn!(
        compact_error = %err,
        "remote compaction failed; compacting locally instead",
    );
    // 说一声。远端那边已经报过一条 Error 事件了，不接着说的话，用户看到的是一条
    // 刺眼的失败然后会话莫名其妙继续——那比失败本身更让人不敢往下用。
    sess.send_event(
        &turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!("Remote compaction failed; compacting locally instead. {err:#}"),
        }),
    )
    .await;
    emit_compact_metric(
        &sess.services.session_telemetry,
        "local",
        /*manual*/ false,
    );

    compact_locally_until_it_works(sess, turn_context, injection, reason, phase).await
}

/// 本地压缩，一直试到成功或者用户按 Esc。
///
/// 上游那个循环封顶 5 次、退避只有几秒（`compact.rs` 的 `retries < max_retries`），
/// 对"模型满载"这类错误等于没等——几秒之内撞完五次然后失败，而上下文已经满了，
/// 整轮就走不下去。这正是 `Selected model is at capacity. Please try a different
/// model.` 那条报错的实际走向：远端 `is_retryable(ServerOverloaded) == false` 当场
/// 返回、一次远端重试都没有，退回本地又只有五次快撞。
///
/// 所以在外面再套一层没有上限的等待。等的是对面腾出容量，而这件事只会随时间好转，
/// 不需要用户做任何决定——需要他决定的只有"还要不要继续"，那就是 Esc。
///
/// 只报一次：上游那个循环自己会在每轮末尾发一条 Error 事件，我们再逐次刷同一句话
/// 等于把提示自己淹掉。所以我们的那句只在第一次失败时说，之后靠拉长的等待间隔让
/// 界面自己安静下来。
async fn compact_locally_until_it_works(
    sess: &Arc<Session>,
    turn_context: Arc<TurnContext>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    /// 第一次失败后等这么久再试。比上游那五次快撞加起来还长——满载是要时间的。
    const FIRST_WAIT: Duration = Duration::from_secs(30);
    /// 等待上限。再长就不像在等，像是死了。
    const MAX_WAIT: Duration = Duration::from_secs(120);

    let mut wait = FIRST_WAIT;
    let mut announced = false;
    loop {
        let attempt = clone_injection(&injection);
        match run_inline_auto_compact_task(
            Arc::clone(sess),
            Arc::clone(&turn_context),
            attempt,
            reason,
            phase,
        )
        .await
        {
            Ok(()) => return Ok(()),
            // 用户自己叫停：立刻退出，这是唯一的出口。
            Err(err) if crate::responses_retry::ends_the_turn(&err) => return Err(err),
            Err(err) => {
                if !announced {
                    announced = true;
                    sess.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Compaction failed and will keep retrying until it succeeds; \
                                 press Esc to stop. {err:#}"
                            ),
                        }),
                    )
                    .await;
                }
                warn!(compact_error = %err, ?wait, "local compaction failed; waiting to retry");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_WAIT);
            }
        }
    }
}

/// 手动 `/compact` 失败之后的恢复。
///
/// 和自动那条同一个道理，只是本地压缩的入口不同（`run_compact_task` 而不是
/// `run_inline_auto_compact_task`）。手动这条**不会**掐断会话——上游把错误吞成
/// `Ok(None)`——但上下文仍然是满的，用户只看到一句报错、什么也没发生，下一轮照样
/// 会撞上下文。所以这里同样要报账号、退回本地、并且一直试到成功。
pub(crate) async fn recover_manual_compaction(
    sess: &Arc<Session>,
    turn_context: Arc<TurnContext>,
    err: CodexErr,
) -> CodexResult<()> {
    if matches!(
        err.details(),
        CodexErrorDetails::TurnAborted | CodexErrorDetails::Interrupted
    ) {
        return Err(err);
    }
    if crate::responses_retry::is_account_scoped(&err) {
        codex_login::report_account_refused();
    }
    warn!(compact_error = %err, "remote /compact failed; compacting locally instead");
    sess.send_event(
        &turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!("Remote compaction failed; compacting locally instead. {err:#}"),
        }),
    )
    .await;
    emit_compact_metric(&sess.services.session_telemetry, "local", /*manual*/ true);

    const FIRST_WAIT: Duration = Duration::from_secs(30);
    const MAX_WAIT: Duration = Duration::from_secs(120);
    let mut wait = FIRST_WAIT;
    let mut announced = false;
    loop {
        let input = vec![codex_protocol::user_input::UserInput::Text {
            text: turn_context
                .config
                .compact_prompt
                .as_deref()
                .unwrap_or(crate::compact::SUMMARIZATION_PROMPT)
                .to_string(),
            text_elements: Vec::new(),
        }];
        match crate::compact::run_compact_task(
            Arc::clone(sess),
            Arc::clone(&turn_context),
            input,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) if crate::responses_retry::ends_the_turn(&err) => return Err(err),
            Err(err) => {
                if !announced {
                    announced = true;
                    sess.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Compaction failed and will keep retrying until it succeeds; \
                                 press Esc to stop. {err:#}"
                            ),
                        }),
                    )
                    .await;
                }
                warn!(compact_error = %err, ?wait, "local /compact failed; waiting to retry");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_WAIT);
            }
        }
    }
}
