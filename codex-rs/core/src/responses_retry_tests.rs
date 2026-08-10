use super::ResponsesStreamRequest;
use super::ends_the_turn;
use super::is_account_scoped;
use super::log_retry;
use super::retry_is_allowed;
use super::sidelines_the_account;
use super::will_not_fix_itself;
use crate::session::tests::make_session_and_context;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::UnexpectedResponseError;
use std::time::Duration;
use tracing_test::internal::MockWriter;

fn http_error(status: u16) -> CodexErr {
    CodexErr::UnexpectedStatus(UnexpectedResponseError {
        status: http::StatusCode::from_u16(status).expect("valid status"),
        body: String::new(),
        user_message: None,
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    })
}

fn sampling_retries(err: &CodexErr) -> bool {
    retry_is_allowed(err, 500, 5, ResponsesStreamRequest::Sampling, /*unbounded*/ true)
}

/// 采样路径上没有次数上限。断开一个跑到一半的会话，代价永远高于多等一会儿。
#[test]
fn a_sampling_request_keeps_retrying_past_every_limit() {
    for err in [
        CodexErr::ServerOverloaded,
        CodexErr::InternalServerError,
        CodexErr::Stream("disconnected".to_string()),
        CodexErr::Timeout,
        CodexErr::QuotaExceeded,
        CodexErr::UsageNotIncluded,
        http_error(404),
    ] {
        assert!(sampling_retries(&err), "{err} should keep retrying");
    }
}

/// **只有用户自己按下的暂停能终结一轮。**
///
/// 2026-08-09 起这张表就剩两个。它们要是也重试，Esc 就失灵了，而"用户随时可以自己
/// 叫停"正是整套无限重试能成立的前提——把这个出口堵上，剩下的设计全部变成挂死。
#[test]
fn only_the_user_ends_a_turn() {
    for err in [CodexErr::TurnAborted, CodexErr::Interrupted] {
        assert!(ends_the_turn(&err), "{err} should end the turn");
        assert!(!sampling_retries(&err), "{err} should not be retried");
    }
}

/// 曾经在那张表里的三个，现在都要重试。
///
/// 三条论据当初听起来都成立，实际都不成立——而它们错判的代价是把一轮跑到一半的
/// 会话直接掐断：
///
/// - `CyberPolicy`："安全判定是答复不是故障"。前提是判定确定性成立，实际这个分类器
///   误判很常见，同样内容重发经常就过。
/// - `ContextWindowExceeded`："重试一万次还是装不下"。前提是中间不压缩。
/// - `SessionBudgetExceeded`：用户自设上限。这一个拿掉确实让那个配置失效，是按
///   "除了 Esc 都别停"的口径有意为之，恢复方式写在 `ends_the_turn` 的注释里。
#[test]
fn the_three_former_hard_walls_now_retry() {
    for err in [
        CodexErr::new(CodexErrorDetails::CyberPolicy {
            message: "refused".to_string(),
        }),
        CodexErr::ContextWindowExceeded,
        CodexErr::SessionBudgetExceeded,
    ] {
        assert!(!ends_the_turn(&err), "{err} must no longer end the turn");
        assert!(sampling_retries(&err), "{err} should keep retrying");
    }
}

/// 远端压缩是一轮**里面**的一步，不是会话本身，而且它失败有本地压缩兜底。在那里
/// 无限等会把整轮挂死，还顺手挡掉兜底——所以只有它保留次数上限。
#[test]
fn remote_compaction_keeps_its_ceiling() {
    let err = CodexErr::Stream("disconnected".to_string());
    assert!(retry_is_allowed(
        &err,
        4,
        5,
        ResponsesStreamRequest::RemoteCompactionV2,
        /*unbounded*/ true
    ));
    assert!(!retry_is_allowed(
        &err,
        5,
        5,
        ResponsesStreamRequest::RemoteCompactionV2,
        /*unbounded*/ true
    ));
    // 池子枯竭是例外：那时候压缩也该等着，因为等的是有人加号，不是一个坏掉的请求。
    assert!(retry_is_allowed(
        &CodexErr::ServerOverloaded,
        500,
        5,
        ResponsesStreamRequest::RemoteCompactionV2,
        /*unbounded*/ true
    ));
}

/// `stream_max_retries` 是退出开关：用户自己设过，就一字不差地回到上游那套。
///
/// 这样"我不想要无限重试"是一句配置的事，不必改代码或者换回官方 codex。
#[test]
fn an_explicit_ceiling_is_honored() {
    let err = CodexErr::Stream("disconnected".to_string());
    let capped = |retries| {
        retry_is_allowed(
            &err,
            retries,
            5,
            ResponsesStreamRequest::Sampling,
            /*unbounded*/ false,
        )
    };
    assert!(capped(4));
    assert!(!capped(5));
}

/// 没有池子时，计费失败不是账号级的。
///
/// 一个号的 `QuotaExceeded`（"检查你的套餐和账单"）不随额度窗口重置恢复，手上又只有
/// 这一个号，换无可换——它照样重试，但要按"不会自己好"来说。
///
/// **有池子时结论相反**，见 `is_account_scoped` 上的说明：池子里另一个号有它自己的
/// 额度和账单。这个 helper 造的 `TurnContext` 没有装 external auth，所以这里断言的
/// 正是"无池子"那一半。
#[test]
fn billing_failures_are_not_account_scoped_without_a_pool() {
    // 这个进程里池子没有派过号（`held_account_email()` 是 None），所以断言的是
    // "无池子"那一半。
    assert!(!is_account_scoped(&CodexErr::QuotaExceeded));
    assert!(!is_account_scoped(&CodexErr::ServerOverloaded));
    assert!(!is_account_scoped(&CodexErr::Stream("x".to_string())));
    assert!(will_not_fix_itself(&CodexErr::QuotaExceeded, true));
}

/// 措辞分档：4xx 说的是"你发的请求有问题"，重试同一个请求得到同一个答案；
/// 408/409/425/429 说的是"现在不行"，不是"这样不行"。
#[test]
fn a_client_error_is_stuck_but_a_busy_signal_is_not() {
    for status in [400, 401, 403, 404, 422] {
        assert!(will_not_fix_itself(&http_error(status), true), "{status}");
    }
    for status in [408, 409, 425, 429, 500, 502, 503] {
        assert!(!will_not_fix_itself(&http_error(status), true), "{status}");
    }
    // 走 WebSocket 时状态码不作数：一次升级被 404 掉说的是"这条通道走不通"，回退到
    // HTTPS 自己会解决，不该让用户去改配置。
    assert!(!will_not_fix_itself(&http_error(404), false));
}

/// 网络类失败要说成"在等它回来"，不是"去改配置"——两句话让用户做的事完全不同。
#[test]
fn a_network_failure_is_not_reported_as_something_to_fix() {
    assert!(!will_not_fix_itself(&CodexErr::Timeout, true));
    assert!(!will_not_fix_itself(&CodexErr::RequestTimeout, true));
    assert!(!will_not_fix_itself(&CodexErr::InternalServerError, true));
    assert!(!will_not_fix_itself(&CodexErr::Stream("x".to_string()), true));
    // 代理返回 HTML 而不是 JSON——这个不会自己好。
    assert!(will_not_fix_itself(&CodexErr::InvalidRequest("x".to_string()), true));
}

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}

/// 换号 ≠ 让服务端把号雪藏起来。
///
/// 429 是 TPM/RPM 每分钟限速，约一分钟就恢复；而客户端能发的 `usage_limit` 让服务端
/// 等整个额度窗口重置（小时级）。把前者报成后者，等于为一分钟的拥塞雪藏一个好号——
/// 并发高时这会以分钟级速度掏空整个池子，然后 `POOL_EXHAUSTED` 让一切变成 30 秒
/// 轮询，表现为"卡住不动"。
///
/// 所以 429 仍然算账号级（换个号继续，另一个号有自己的 TPM 桶），但**不上报**。
#[test]
fn a_rate_limit_swaps_accounts_without_sidelining_one() {
    let four_two_nine = CodexErr::RetryLimit(codex_protocol::error::RetryLimitReachedError {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        request_id: None,
    });
    // 没有池子时它连账号级都不算（换无可换）。
    assert!(!is_account_scoped(&four_two_nine));
    // 但无论如何，它永远不该被报成"这个号的额度窗口用尽了"。
    assert!(!sidelines_the_account(&four_two_nine));

    // 真正的额度用尽反过来：值得雪藏。
    assert!(sidelines_the_account(&CodexErr::UsageLimitReached(
        codex_protocol::error::UsageLimitReachedError {
            plan_type: None,
            resets_at: None,
            rate_limits: None,
            promo_message: None,
            rate_limit_reached_type: None,
        }
    )));
}
