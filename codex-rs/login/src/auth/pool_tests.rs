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
    ledger.refusal = None;
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
            row.account_key.as_deref() == Some(account_key) && row.model.as_deref() == Some(model)
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
    assert_eq!(
        row_for(&reported, "acct-a", "gpt-5-codex").total_tokens,
        300
    );
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

    assert_eq!(super::refusal_reason(), None);
    super::report_account_refused("http_403");
    // 读它不销账：请求可能失败，那时候还得再报一次。
    assert_eq!(super::refusal_reason().as_deref(), Some("http_403"));
    assert_eq!(super::refusal_reason().as_deref(), Some("http_403"));
    super::clear_refusal();
    assert_eq!(super::refusal_reason(), None, "送达之后不能再报第二遍");
}

/// 原因本身要带上，而不只是"被拒了"这一个比特。
///
/// 以前这里是个 `bool`，于是 403 被停用、402 计费、5xx 在服务端看来和配额用尽长得
/// 一模一样——实际上它们该被完全不同地处置。
#[test]
fn the_refusal_carries_the_reason_not_just_a_flag() {
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    super::report_account_refused(super::REJECT_USAGE_LIMIT);
    assert_eq!(
        super::refusal_reason().as_deref(),
        Some(super::REJECT_USAGE_LIMIT)
    );

    // 后来的覆盖先前的：同一次失败会被重试几轮，最后一次最接近现状。
    super::report_account_refused("retry_limit_429");
    assert_eq!(super::refusal_reason().as_deref(), Some("retry_limit_429"));

    // 空原因不记：记下来会让下一次派号带上一个空 reject，服务端只能当没说。
    super::report_account_refused("   ");
    assert_eq!(super::refusal_reason().as_deref(), Some("retry_limit_429"));
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
    provider_wanting(server, None)
}

fn provider_wanting(
    server: &MockServer,
    want_account: Option<&str>,
) -> (PoolAuth, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let pool = PoolAuth::new(Config {
        base_url: server.uri(),
        key: "test-key".to_string(),
        device_id: "test-device".to_string(),
        want_account: want_account.map(str::to_string),
    });
    (pool, home)
}

/// 点名的号必须真的出现在派号请求里。
///
/// `body_partial_json` 只在请求体确实带了这个字段时才匹配；匹配不上 wiremock
/// 直接不回，`resolve()` 会失败——所以这条测试是吃劲的，不是"跑通就算"。
#[tokio::test]
async fn a_wanted_account_is_sent_with_every_lease_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .and(body_partial_json(serde_json::json!({
            "want_account": "someone@example.com"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider_wanting(&server, Some("someone@example.com"));
    pool.current(/*allow_stale*/ true)
        .await
        .expect("pool should serve the wanted account");
}

/// 没点名时**不能**发这个字段。
///
/// 发一个空串上去，服务端那边 `str(payload.get('want_account') or '').strip()`
/// 虽然扛得住，但让"没点名"和"点名要一个空串"在网络上长得一样，迟早有人照着
/// 空串去查。
#[tokio::test]
async fn no_wanted_account_means_the_field_is_absent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .mount(&server)
        .await;

    let (pool, _home) = provider_wanting(&server, None);
    pool.current(/*allow_stale*/ true).await.expect("lease");

    let requests = server.received_requests().await.expect("requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("json body");
    assert!(body.get("want_account").is_none(), "{body}");
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
    // `install_if_configured` 会 `attach_ledger`，把进程级账本的 `rows` 整个换成
    // 磁盘上读到的（这个临时 HOME 里是空的）。不拿这把锁，它就会在别的测试
    // 记完账、还没断言之前把账清掉——`the_last_turn_rides_out_on_the_release`
    // 就是这样在并行下随机变红的。
    let _ledger_guard = LEDGER_TEST_LOCK.lock().expect("test lock");
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

/// 上面那个测试盯的是 `shared()`，而 `codex` 一个真实入口都不走 `shared()`。
///
/// 这条盯的是真正被走的那条路。0.147.0 上游在 `shared_from_config` 和 `shared()`
/// 之间插了 `shared_from_auth_config`，`shared()` 本身一个字没动——合并没有冲突，
/// 代码照编，上面那个测试照过，而池子在 `cli/main.rs`、`app-server/in_process.rs`
/// 和 TUI 里全部静默失效，codext 每次退回本机 auth.json。用户看到的是"启动起来
/// 跟原生 codex 一模一样"，因为那时候它确实就是。
///
/// 所以这条测试的意义不在于多测一个构造函数，而在于：**挂钩点的正确性只能由走
/// 真实入口的测试来保证，不能由"这个函数近几版没被改过"来保证。** 上游改的从来
/// 不必是我们挂的那个函数，改调用图就够了。
#[tokio::test]
async fn the_pool_is_installed_on_the_path_the_cli_actually_takes() {
    // `install_if_configured` 会 `attach_ledger`，把进程级账本的 `rows` 整个换成
    // 磁盘上读到的（这个临时 HOME 里是空的）。不拿这把锁，它就会在别的测试
    // 记完账、还没断言之前把账清掉——`the_last_turn_rides_out_on_the_release`
    // 就是这样在并行下随机变红的。
    let _ledger_guard = LEDGER_TEST_LOCK.lock().expect("test lock");
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

    // `AuthManager::shared_from_config` 只是把 `Config` 拆成 `AuthConfig` 再调这个；
    // `login` 不能依赖 `core`（会成环），所以在这一层能测到的汇合点就是它。
    let manager = crate::auth::AuthManager::shared_from_auth_config(
        crate::auth::AuthConfig {
            codex_home: home.path().to_path_buf(),
            auth_credentials_store_mode: crate::AuthCredentialsStoreMode::File,
            keyring_backend_kind: crate::auth::AuthKeyringBackendKind::default(),
            forced_login_method: None,
            chatgpt_base_url: None,
            forced_chatgpt_workspace_id: None,
            managed_auth_policy: Default::default(),
            auth_route_config: crate::test_support::transport_default_auth_route_config(),
        },
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .expect("auth manager initialization");

    assert!(
        manager.has_external_auth(),
        "pool provider must be installed on the shared_from_config path too — \
         that is the one every codex entry point uses"
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

/// 派号失败之后，**只要上一层还会继续用这个号，就不能声称"手上没号"**。
///
/// 这一条修的是一个撕裂状态。`AuthManager::load_auth` 在外部凭据解析失败时写着
/// `// Keep serving the last known credential for this call;`——手上还有缓存凭据
/// 时它继续用这个池子的号发请求。而这边原来无条件 `forget_lease()`，于是进程
/// **正在用**这个号、却对外声称手上没有号。
///
/// 后果不是"少记一笔"：`RetryKind::of` 判 429 要不要换号、要不要上报，靠的就是
/// `held_account_email().is_some()`。声称没号 ⇒ 429 被归成网络故障 ⇒ 无限重试、
/// 不上报、不换号，而且不会自己好。2026-08-31 线上那条卡了十六分钟的会话就是它。
#[tokio::test]
async fn a_failed_lease_keeps_the_held_account_while_the_credential_is_still_in_use() {
    // 这条测试读写进程级账本（`held_account_email` / `report_account_refused`），
    // 必须和其它动账本的测试串行，否则互相踩，而且第一个 panic 会毒化这把锁、
    // 让后面十几条全部变成 PoisonError，真正的失败被埋掉。
    let _ledger_guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);
    let server = MockServer::start().await;
    let good = Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_body("acct-a")))
        .expect(1)
        .named("first lease")
        .mount_as_scoped(&server)
        .await;

    let (pool, _home) = provider(&server);
    pool.current(true).await.expect("initial lease");
    assert_eq!(super::held_account_email().is_some(), true);
    drop(good);

    // 之后派号一律失败，而且带着一条待报的拒绝——正是 429 之后的那个状态：
    // 合并窗被绕过、`allow_stale` 也救不了，一定走到放弃分支。
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    super::report_account_refused("retry_limit_429");
    next_request(&pool);
    let _ = pool.current(true).await;

    // 上一层仍然拿得到这份凭据，所以身份必须还在。
    assert!(
        pool.cached_auth().is_some(),
        "上一层要继续用的那份凭据不该被丢掉",
    );
    assert!(
        super::held_account_email().is_some(),
        "还在用这个号就不能声称手上没号——429 会因此被归成网络故障",
    );
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
    // `install_if_configured` 会 `attach_ledger`，把进程级账本的 `rows` 整个换成
    // 磁盘上读到的（这个临时 HOME 里是空的）。不拿这把锁，它就会在别的测试
    // 记完账、还没断言之前把账清掉——`the_last_turn_rides_out_on_the_release`
    // 就是这样在并行下随机变红的。
    let _ledger_guard = LEDGER_TEST_LOCK.lock().expect("test lock");
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

/// 号池派下来的两种凭据必须落到**各自的**认证模式上。
///
/// 这不是分类洁癖：上游的 `supports_unauthorized_recovery` 把
/// `ChatgptAuthTokens` 列为"401 之后去恢复"，而 PAT 不在表里。PAT 在
/// `plugins/featured` / `rate-limit-reset-credits` / `models` 上本来就是 401
/// （2026-08-16 实测），一旦被当成 ChatgptAuthTokens，那几个附属调用的 401
/// 就会不停地去重新要号，每 1.3 秒一轮，`responses` 一次也发不出去。
#[test]
fn an_opaque_pool_token_becomes_personal_access_token_auth() {
    let data: super::LeaseData = serde_json::from_str(
        r#"{"account_key":"user-HEmy30VWx07XyeNh3STGa8bK::74ffdf33-9204-4797-9b00-2a6120b4f91c",
            "plan":"team",
            "auth_json":{"tokens":{"id_token":"","access_token":"at-Mq7xOpaque",
                                   "account_id":"74ffdf33-9204-4797-9b00-2a6120b4f91c"}}}"#,
    )
    .expect("lease data");

    let auth = PoolAuth::validate(&data).expect("PAT 必须能构造出凭据");
    assert!(
        auth.is_personal_access_token_auth(),
        "at- 开头的令牌要走 PAT 模式，否则会打开 401 恢复循环"
    );
    // 账号归属来自服务端下发的字段，不是从令牌里解出来的。
    assert_eq!(
        auth.get_account_id().as_deref(),
        Some("74ffdf33-9204-4797-9b00-2a6120b4f91c")
    );
}

#[test]
fn a_jwt_pool_token_still_becomes_chatgpt_auth_tokens() {
    let data: super::LeaseData = serde_json::from_str(
        r#"{"account_key":"user-abc::acct-def","plan":"plus",
            "auth_json":{"tokens":{"access_token":"eyJhbGciOiJub25lIn0.eyJhIjoxfQ.sig",
                                   "account_id":"acct-def"}}}"#,
    )
    .expect("lease data");

    let auth = PoolAuth::validate(&data).expect("OAuth 令牌照旧");
    assert!(auth.is_external_chatgpt_tokens(), "JWT 的那条路一字不能变");
}

/// account_key 的前半段就是 chatgpt_user_id；格式不对时留空而不是编一个。
#[test]
fn the_user_id_comes_from_the_account_key() {
    assert_eq!(PoolAuth::user_id_of("user-1::acct-2"), "user-1");
    assert_eq!(PoolAuth::user_id_of("没有分隔符"), "");
}

/// 释放必须打 `/release`，**不能**打 `/lease`。
///
/// `/lease` 的语义是「派一个号给我」，服务端会为它建一份新租约——拿它来释放等于
/// 刚交回名额就又占一个，净效果是零，而且看日志还以为释放成功了。
///
/// 这条测试是吃劲的：`/lease` 那个 mock 期望被调用 0 次，wiremock 在 drop 时校验。
#[tokio::test]
async fn releasing_does_not_go_through_the_lease_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/release"))
        .and(body_partial_json(serde_json::json!({
            "device_id": "test-device"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {"released": true}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let (pool, _home) = provider_wanting(&server, None);
    pool.release().await.expect("release should reach the pool");
}

/// 最后一轮用量跟着释放一起走。
///
/// 20 秒的心跳只保证「跑着的时候不会积压太久」，退出前最后那一轮它未必赶得上；
/// 账本虽然落盘留给下一次运行补报，但"最后一次"常常真的就是最后一次。
#[tokio::test]
async fn the_last_turn_rides_out_on_the_release() {
    // 账本是进程级静态，不拿这把锁就会看见别的测试的账（或者被它们清空）。
    let _guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {"released": true}
        })))
        .mount(&server)
        .await;

    reset_ledger(None);
    super::set_held_account(Some("acct-a".to_string()), None);
    super::record_turn_usage("session-z", "gpt-5.5", &turn(1234));

    let (pool, _home) = provider_wanting(&server, None);
    pool.release().await.expect("release");

    let requests = server.received_requests().await.expect("requests");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
    let sessions = body["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("release body carried no sessions: {body}"));
    assert_eq!(sessions.len(), 1, "{body}");
    assert_eq!(sessions[0]["session_id"], "session-z", "{body}");
    assert_eq!(sessions[0]["total_tokens"], 1234, "{body}");
    reset_ledger(None);
}

/// 退出钩子必须真的挂在 `cli` 的 `main` 上。
///
/// 这条测试存在的唯一理由是 0.147.0 那次事故：`install_if_configured` 挂在
/// `AuthManager::shared()` 上，上游在中间插了一层把调用改道，`shared()` 本身一个
/// 字没动、合并零冲突、代码照编，而挂钩变成了死代码——表现是"codext 跑起来跟原生
/// 一模一样"。**按函数级 churn 挑得中「这个函数会不会被改」，挑不中「谁还会调用
/// 它」**，所以每个挂钩点都要有一条测试盯着它还在不在。
///
/// 读源码而不是跑二进制，是因为这里要证明的就是"那一行还在源码里"。上游把 `main`
/// 的实现搬走时这条会红——那正是我们要的：合并时被迫重新看一眼，而不是静默失效。
#[test]
fn the_exit_hook_is_wired_into_the_cli_entry_point() {
    const CLI_MAIN: &str = include_str!("../../../cli/src/main.rs");
    assert!(
        CLI_MAIN.contains("release_on_exit"),
        "cli/src/main.rs 不再调用 release_on_exit：退出时不会交回调度名额，\
         已退出的会话会一直占着并发除数直到服务端的租约 TTL 到期"
    );
}

/// 启动那一刻租不到号，**不等于**这个进程一辈子用不上池子。
///
/// 这是 2026-08-25 真实事故的回归用例：`install_if_configured` 只在进程启动时跑一次，
/// 而它当时把「装 provider」绑在了「此刻能不能 resolve 出凭据」上。于是启动瞬间池子
/// 抖一下（或恰好没号可派），provider 就永远装不上，`has_external_auth()` 恒为 false，
/// core 的 `RetryKind::of` 据此判成「没有池子可换」，手上那个已经打满的本机号一路重试
/// 到配额窗口重置——实测钉死 7.5 小时，其时池子是健康的、别的会话都在正常换号。
///
/// 现有用例（`an_unreachable_pool_does_not_drop_a_working_lease`、
/// `an_empty_pool_does_not_keep_riding_the_old_lease`）覆盖的都是**已经有租约之后**的
/// 抖动，恰好漏掉了启动这一刻——那时 `cached_auth()` 是空的，「连不上就继续骑旧租约」
/// 那条兜底根本不成立。
#[tokio::test]
async fn a_pool_that_cannot_serve_at_startup_is_still_installed_and_recovers() {
    let _ledger_guard = LEDGER_TEST_LOCK.lock().expect("test lock");
    reset_ledger(None);

    let server = MockServer::start().await;
    // 第一次问：`data: null`——池子此刻没号可派（真事故里是网络抖动，两者在
    // `current()` 里都归到 `Err`，走同一条安装路径）。
    Mock::given(method("POST"))
        .and(path("/x8Rk3Nq6Vd2/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_pool_body()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // 之后池子恢复供号。
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

    let manager = crate::auth::AuthManager::shared_from_auth_config(
        crate::auth::AuthConfig {
            codex_home: home.path().to_path_buf(),
            auth_credentials_store_mode: crate::AuthCredentialsStoreMode::File,
            keyring_backend_kind: crate::auth::AuthKeyringBackendKind::default(),
            forced_login_method: None,
            chatgpt_base_url: None,
            forced_chatgpt_workspace_id: None,
            managed_auth_policy: Default::default(),
            auth_route_config: crate::test_support::transport_default_auth_route_config(),
        },
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .expect("auth manager initialization");

    // 这一条是整个修复的要害：启动没租到号，provider 也必须装上。装不上的话
    // `leases_credentials()` 为 false，配额用尽就再也换不了号了。
    assert!(
        manager.has_external_auth(),
        "启动这一次没租到号，provider 仍然必须装上——否则这个进程一辈子换不了号，\
         本机那个号打满之后只能干等配额窗口重置"
    );

    // 而且它必须真的能自愈：下一次取凭据重新问池子，号回来了就切回池子。
    let auth = manager
        .auth()
        .await
        .expect("池子恢复供号之后，下一次取凭据就该拿到池子的号");
    assert_eq!(
        auth.get_account_id().as_deref(),
        Some("acct-pool"),
        "自愈之后凭据必须来自池子，而不是继续用本机 auth.json"
    );
}
