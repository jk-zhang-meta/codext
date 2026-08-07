use base64::Engine;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::Config;
use super::PoolAuth;

/// 不经 ags 直接跑 codext 时，地址和密钥要能从 `CODEX_HOME/pool.json` 读到。
#[test]
fn the_pool_is_configurable_without_environment_variables() {
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        home.path().join("pool.json"),
        r#"{"base_url": "https://pool.example.com:844/", "key": "  k-123  "}"#,
    )
    .expect("write config");

    let config = Config::load(home.path()).expect("config should be found");
    // 末尾斜杠会和拼接出来的路径前缀撞成 `//`，密钥两头的空白会被原样发出去。
    assert_eq!(config.base_url, "https://pool.example.com:844");
    assert_eq!(config.key, "k-123");
}

/// device_id 必须**只跟工作目录有关**，不能带进程号。
///
/// 带进程号的话每次调用都是一个新租约，而租约要挂满 TTL 才过期：一次 `doctor`
/// 加一次 `exec` 就留下两个幽灵持有者，它们会抬高调度的并发除数、把派号算歪。
#[test]
fn the_device_id_is_stable_across_runs_in_the_same_workspace() {
    let first = super::workspace_tag();
    let second = super::workspace_tag();
    assert_eq!(first, second, "同一个目录两次调用必须得到同一个 device_id");
    assert!(
        !first.contains(&std::process::id().to_string()),
        "device_id 不能带进程号，否则每次调用都会新占一个租约"
    );
}

/// 账本是进程级静态，这些测试必须串行，否则一个测试的账会被另一个看见。
static LEDGER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn reset_ledger(path: Option<std::path::PathBuf>) {
    let mut ledger = super::LEDGER.lock().expect("ledger lock");
    ledger.path = path;
    ledger.held = None;
    ledger.refused = false;
    ledger.rows.clear();
}

/// 一次调用的用量。分不分到 input/output 不影响这些断言，看的是 total 和归属。
fn turn(total: i64) -> codex_protocol::protocol::TokenUsage {
    codex_protocol::protocol::TokenUsage {
        input_tokens: total,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        total_tokens: total,
        // 上游 0.147.0 新加的字段：厂商报的 rollout 预算消耗。账本只按 token 数
        // 和归属记账，跟它无关。
        codex_rollout_budget_units: None,
    }
}

fn row_for<'a>(
    reported: &'a [super::SessionUsage],
    account_key: &str,
    model: &str,
) -> &'a super::SessionUsage {
    reported
        .iter()
        .find(|row| {
            row.account_key.as_deref() == Some(account_key)
                && row.model.as_deref() == Some(model)
        })
        .unwrap_or_else(|| panic!("{account_key}/{model} 应该有一条账：{reported:?}"))
}

/// 用量必须记在**当时服务它的那个号**头上。
///
/// 这是整套统计的核心断言。旧做法是事后扫 rollout 反推归属，一个本地没有记录的
/// 会话会从 0 起算，把它的整段历史一次性算给换号后手上的那个号——线上因此出现过
/// 「跑满 100% 却零请求」和「记了两千多次请求却一点额度都没动」这两头对称的错。
#[test]
fn usage_follows_the_account_that_served_the_turn() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    super::set_held_account(Some("acct-a".to_string()), None);
    super::record_turn_usage("s-1", "gpt-5", &turn(100));
    super::record_turn_usage("s-1", "gpt-5", &turn(50));
    super::set_held_account(Some("acct-b".to_string()), None);
    super::record_turn_usage("s-1", "gpt-5", &turn(7));

    let reported = super::pending_usage();
    let first = row_for(&reported, "acct-a", "gpt-5");
    assert_eq!((first.requests, first.total_tokens), (2, 150));
    let second = row_for(&reported, "acct-b", "gpt-5");
    assert_eq!(
        (second.requests, second.total_tokens),
        (1, 7),
        "换号之后的用量不能倒灌回上一个号"
    );
}

/// 同一个号跑了两个模型，两段要分开——计价按模型走，混在一起就没法算钱了。
#[test]
fn each_model_keeps_its_own_line() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    super::set_held_account(Some("acct-a".to_string()), None);
    super::record_turn_usage("s-1", "gpt-5", &turn(10));
    super::record_turn_usage("s-1", "gpt-5-codex", &turn(300));

    let reported = super::pending_usage();
    assert_eq!(row_for(&reported, "acct-a", "gpt-5").total_tokens, 10);
    assert_eq!(row_for(&reported, "acct-a", "gpt-5-codex").total_tokens, 300);
}

/// 没租到号的时候跑的用量不归池子。
///
/// 少了这一条，退回本机 auth.json 期间的用量会被栽到最后持有过的那个号头上。
#[test]
fn usage_without_a_lease_is_charged_to_nobody() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    super::set_held_account(None, None);
    super::record_turn_usage("s-1", "gpt-5", &turn(999));

    assert!(super::pending_usage().is_empty());
}

/// 拒绝要一直留到真的送达为止，送达之后只报一次。
///
/// 如果在发请求**之前**就销账，一次网络失败就永久丢掉这条消息：服务端再也不知道
/// 这个号满了，于是一次次把它发回给正在重试的会话——正是要修的那个死循环。
#[test]
fn a_refusal_survives_until_it_is_delivered() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    assert!(!super::refusal_pending());
    super::report_account_refused();
    // 读它不销账：请求可能失败，那时候还得再报一次。
    assert!(super::refusal_pending());
    assert!(super::refusal_pending());
    super::clear_refusal();
    assert!(!super::refusal_pending(), "送达之后不能再报第二遍");
}

/// 账本要能跨进程重启接上。
///
/// 服务端按 (会话, 号, 模型) 取较大值来保证重发幂等；一个重启后从 0 重新累计的
/// 进程会一直报出比库里更小的数，那之后的新用量就再也盖不过去了。
#[test]
fn the_ledger_survives_a_restart() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    let home = tempfile::tempdir().expect("tempdir");
    reset_ledger(Some(home.path().join(super::LEDGER_FILE)));

    super::set_held_account(Some("acct-a".to_string()), None);
    super::record_turn_usage("s-1", "gpt-5", &turn(120));

    // 换个进程：内存清空，只从文件恢复。
    reset_ledger(None);
    assert!(super::pending_usage().is_empty());
    super::attach_ledger(home.path());

    let reported = super::pending_usage();
    assert_eq!(row_for(&reported, "acct-a", "gpt-5").total_tokens, 120);
}

/// 报完不删：重发是空操作，而「发出去了但回包丢了」在客户端看来和没发一样。
#[test]
fn reporting_does_not_clear_the_ledger() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    super::set_held_account(Some("acct-a".to_string()), None);
    super::record_turn_usage("s-1", "gpt-5", &turn(5));

    let first = super::pending_usage();
    let again = super::pending_usage();
    assert_eq!(first, again);
    assert_eq!(first.len(), 1);
}

/// 没配就必须返回 None —— 那是「退回上游本地 auth.json」的信号，不是错误。
#[test]
fn an_unconfigured_home_leaves_upstream_auth_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    assert!(Config::load(home.path()).is_none());
}

fn fake_access_token() -> String {
    fake_jwt("acct-pool")
}

/// 一个能被解析出账号 id 的 JWT。签名不作数，上游只读 claims。
fn fake_jwt(account_id: &str) -> String {
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = b64(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "email": format!("{account_id}@example.com"),
        "https://api.openai.com/auth": {
            "chatgpt_user_id": format!("user-{account_id}"),
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": "plus",
        },
    });
    let payload = b64(&serde_json::to_vec(&payload).expect("payload"));
    format!("{header}.{payload}.{}", b64(b"sig"))
}

/// 本机自己 `codex login` 登出来的那份凭据。
fn write_local_auth(codex_home: &std::path::Path, account_id: &str) {
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": fake_jwt(account_id),
                "access_token": "local-access-token",
                "refresh_token": "local-refresh-token",
                "account_id": account_id,
            },
            "last_refresh": chrono::Utc::now(),
        })
        .to_string(),
    )
    .expect("write auth.json");
}

/// 服务端的「此刻没号可发」：`code` 是 0，`data` 是 null。
fn empty_pool_body() -> serde_json::Value {
    serde_json::json!({"code": 0, "data": null})
}

fn lease_body(account_key: &str) -> serde_json::Value {
    serde_json::json!({
        "code": 0,
        "data": {
            "account_key": account_key,
            "plan": "plus",
            "auth_json": {
                "tokens": {
                    "access_token": fake_access_token(),
                    "account_id": "acct-pool",
                }
            }
        }
    })
}

/// 把上次决策推到合并窗口之外，等价于「过了一会儿又来一个请求」。
///
/// 直接改时钟而不是 sleep：一个真睡一秒的测试没人愿意留着。
fn next_request(pool: &PoolAuth) {
    let mut guard = pool.lease.write().expect("lease lock");
    if let Some(lease) = guard.as_mut() {
        lease.decided_at = std::time::Instant::now() - super::DECISION_COALESCE;
    }
}

fn provider(server: &MockServer) -> (PoolAuth, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let pool = PoolAuth::new(Config {
        base_url: server.uri(),
        key: "test-key".to_string(),
        device_id: "test-device".to_string(),
    });
    (pool, home)
}

/// 整套东西的关键一环，也是唯一能证明"接进去了"的测试。
///
/// 装上 provider 之后 `AuthManager` 必须从池子取凭据——注意这个 `CODEX_HOME` 里
/// **没有 auth.json**，凭据完全来自网络。
///
/// 别再拿 `codex login status` 或 `codex doctor` 验这件事：那两个命令分别走
/// `CodexAuth::from_auth_storage()` 和 `load_auth_dot_json()`，都是直接读磁盘、
/// 压根不构造 `AuthManager`，所以它们永远显示"未登录"，跟池子通不通没有关系。
#[tokio::test]
async fn the_auth_manager_serves_pool_credentials_with_no_auth_json_on_disk() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        home.path().join("pool.json"),
        serde_json::json!({"base_url": server.uri(), "key": "test-key"}).to_string(),
    )
    .expect("write config");
    assert!(!home.path().join("auth.json").exists());

    let manager = crate::auth::AuthManager::shared(
        home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        crate::AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        crate::auth::AuthKeyringBackendKind::default(),
        crate::test_support::transport_default_auth_route_config(),
    )
    .await;

    assert!(
        manager.has_external_auth(),
        "pool provider should have been installed by AuthManager::shared"
    );
    let auth = manager
        .auth()
        .await
        .expect("credentials must come from the pool");
    assert_eq!(auth.get_account_id().as_deref(), Some("acct-pool"));
}

/// 一次模型调用周围的连锁取凭据只打一个往返。
///
/// 上游的 `auth()` 在外部 provider 模式下每次都 `reload()`，围绕一次调用会被调
/// 几十次。逐个发请求的话服务端会被当成心跳靶子——实测启动阶段 550 毫秒里就有
/// 十几次。
#[tokio::test]
async fn the_calls_around_one_request_collapse_into_one_pool_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .expect(1)
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    for _ in 0..20 {
        pool.current(/*allow_stale*/ true)
            .await
            .expect("lease should succeed");
    }
    // MockServer 在 drop 时校验 expect(1)。
}

/// 但下一个请求必须重新决策——**没有租约缓存**。
///
/// 这是整套设计的前提。攥着上一次的结果不放，就意味着手上的号额度跑满、被停用、
/// 被冷却之后照样继续用，直到某个计时器到点。合并窗口只吃掉同一个请求内部的重复
/// 提问，吃不掉两个请求之间的那次决策。
#[tokio::test]
async fn the_next_request_gets_a_fresh_decision() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .expect(2)
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("first request");
    // 把上次决策的时刻推到合并窗口之外，等价于「过了一会儿又来一个请求」。
    // 直接改时钟而不是 sleep：一个真睡一秒的测试没人愿意留着。
    next_request(&pool);
    pool.current(true).await.expect("second request");
}

/// 手上那个号要报给服务端——粘性判断在服务端做，靠的就是这个字段。
///
/// 不带上去的话服务端每次都只能当新终端处理、重新挑一个，缓存就没了。刻意不从
/// 服务端的租约表里读：那样服务端一重启、租约行一被清，粘性就断。
#[tokio::test]
async fn the_held_account_is_sent_back_to_the_pool() {
    let server = MockServer::start().await;
    // 第一次没有号可带；带上 account_key 的那次单独匹配，命中即证明带对了。
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .and(body_partial_json(
            serde_json::json!({"device_id": "test-device", "account_key": "acct-a"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("initial lease");
    next_request(&pool);
    pool.current(true).await.expect("second lease");
}

/// 池子连不上不该把正在跑的会话打断：手上的 access token 通常还有十几分钟有效期。
#[tokio::test]
async fn an_unreachable_pool_does_not_drop_a_working_lease() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    let first = pool.current(true).await.expect("initial lease");
    server.reset().await;
    next_request(&pool);

    let reused = pool
        .current(/*allow_stale*/ true)
        .await
        .expect("an unreachable pool must not invalidate the current lease");
    assert_eq!(reused.get_account_id(), first.get_account_id());
}

/// 凭据被 401 拒了之后不能拿旧的顶上——那份刚被对面拒绝，再用一次只会再失败一次。
#[tokio::test]
async fn a_rejected_credential_is_not_served_from_cache() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("initial lease");
    server.reset().await;

    let err = pool.current(/*allow_stale*/ false).await;
    assert!(
        err.is_err(),
        "refresh() after a rejection must surface the failure, not reuse the rejected lease"
    );
}

/// 401 要如实报给服务端，否则那个号会被一次次派回来、一次次 401。
#[tokio::test]
async fn a_401_is_reported_so_the_pool_can_park_the_account() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("initial lease");
    pool.current(/*allow_stale*/ false)
        .await
        .expect("a replacement should come back");

    let reject = server
        .received_requests()
        .await
        .expect("requests recorded")
        .last()
        .map(|request| request.body_json::<serde_json::Value>().expect("json"))
        .and_then(|body| body.get("reject").cloned());
    assert_eq!(reject, Some(serde_json::json!("unauthorized")));
}

/// 「此刻没号可发」和「联系不上」必须分开处置。
///
/// 连不上是抖动，手上那份还能顶一会儿（见上面那个测试）；而服务端明确说没号，说
/// 的就是它不打算再发手上这个了——多半正是因为它额度到顶/被停用/在冷却。这时候继
/// 续骑着只会一路撞墙，还撞不出 401（429 不走 `refresh()`），永远逃不出来。
#[tokio::test]
async fn an_empty_pool_does_not_keep_riding_the_old_lease() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_pool_body()))
        .mount(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("initial lease");
    next_request(&pool);

    assert!(
        pool.current(/*allow_stale*/ true).await.is_err(),
        "「没号可发」是确定的答复，不该被当成抖动而复用旧租约"
    );
    assert!(
        pool.lease.read().expect("lease lock").is_none(),
        "还记着那个号的话，退回本地之后跑掉的用量和额度读数会被记到它头上"
    );
}

/// 池子给不出号的时候退回本机 `auth.json`，而不是让整台机器变成「未登录」。
///
/// 装上 provider 之后上游的 `load_auth()` 原本只问池子，问不出来就返回 None。号池
/// 空了是个正常状态（几个号同时到顶），那时候本机自己登录过的号还在，没有道理不用。
///
/// 退回是**每次调用**的降级，不是切换：provider 还装着，下一次取凭据照样先问池子。
#[tokio::test]
async fn an_empty_pool_falls_back_to_the_local_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_pool_body()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-b")))
        .mount(&server)
        .await;

    let (pool, home) = provider(&server);
    write_local_auth(home.path(), "acct-local");
    let pool = std::sync::Arc::new(pool);

    let manager = crate::auth::AuthManager::shared(
        home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        crate::AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        crate::auth::AuthKeyringBackendKind::default(),
        crate::test_support::transport_default_auth_route_config(),
    )
    .await;
    manager
        .set_external_auth(pool.clone())
        .await
        .expect("the pool serves the first lease");

    let leased = manager.auth().await.expect("the pool has an account");
    assert_eq!(
        leased.get_account_id().as_deref(),
        Some("acct-pool"),
        "有号的时候必须用池子的号，本地那份只是兜底"
    );

    next_request(&pool);
    let fallback = manager.auth().await.expect("号池空了不该让本机变成未登录");
    assert_eq!(
        fallback.get_account_id().as_deref(),
        Some("acct-local"),
        "池子给不出号时应当退回本机 auth.json"
    );
    assert!(
        manager.has_external_auth(),
        "退回是这一次调用的降级，provider 必须还装着，下一次还要先问池子"
    );

    // 池子一有号就必须回到池子上——本机那个号是兜底，不是新的默认。
    next_request(&pool);
    let recovered = manager.auth().await.expect("the pool has an account again");
    assert_eq!(
        recovered.get_account_id().as_deref(),
        Some("acct-pool"),
        "号一回来就该切回池子，退回本地不能变成一条单向路"
    );
}
