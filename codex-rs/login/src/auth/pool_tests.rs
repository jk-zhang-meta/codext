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
        r#"{"base_url": "https://www.itachi.fans:844/", "key": "  k-123  "}"#,
    )
    .expect("write config");

    let config = Config::load(home.path()).expect("config should be found");
    // 末尾斜杠会和拼接出来的路径前缀撞成 `//`，密钥两头的空白会被原样发出去。
    assert_eq!(config.base_url, "https://www.itachi.fans:844");
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

fn write_rollout(dir: &std::path::Path, events: &[serde_json::Value]) -> std::path::PathBuf {
    let path = dir.join("rollout-2026-08-03T00-00-00-11111111-2222-3333-4444-555555555555.jsonl");
    let body: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    std::fs::write(&path, body.join("\n")).expect("write rollout");
    path
}

fn token_count_event(total: u64, used_percent: f64) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-08-03T00:00:00.000Z",
        "payload": {
            "type": "token_count",
            "model": "gpt-5.6",
            "info": {"total_token_usage": {
                "input_tokens": total, "cached_input_tokens": 0,
                "output_tokens": 0, "reasoning_output_tokens": 0,
                "total_tokens": total,
            }},
            "rate_limits": {
                "plan_type": "plus",
                "primary": {"window_minutes": 300, "used_percent": used_percent, "resets_at": 1786000000},
                "secondary": {"window_minutes": 10080, "used_percent": 4.0, "resets_at": 1786600000},
            },
        },
    })
}

/// 累计量取**最后一条**事件，不是把每条加起来。
///
/// `total_token_usage` 本身就是会话累计值，相加等于把消耗乘以回合数——一个长会话
/// 能报出几亿个它根本没用过的 token，然后污染整个池子的统计。请求数是唯一该累加的。
#[test]
fn session_totals_come_from_the_last_event_not_the_sum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        &[token_count_event(1_000, 5.0), token_count_event(2_500, 12.5)],
    );

    let usage = super::read_rollout(&path).expect("rollout should parse");
    assert_eq!(usage.requests, 2, "请求数要累加");
    assert_eq!(usage.total_tokens, 2_500, "token 取最后一条，不是 3500");
    assert_eq!(usage.session_id, "11111111-2222-3333-4444-555555555555");
    let limits = usage.rate_limits.expect("应当带回额度读数");
    assert_eq!(limits.windows.len(), 2, "5h 和周窗都要带上");
    // 留最新那条读数：最后一次调用才反映账号此刻的额度。
    assert_eq!(limits.windows[0].used_percent, 12.5);
    assert_eq!(limits.windows[0].window_minutes, 300);
    assert_eq!(limits.windows[1].window_minutes, 10080);
}

/// 两次刷新之间 OpenAI 会发全 null 的窗口。那不是"用量为零"，不能当读数报上去
/// ——报了会把一个快跑满的号在调度眼里刷成满血。
#[test]
fn all_null_windows_do_not_become_a_zero_usage_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        &[serde_json::json!({
            "timestamp": "2026-08-03T00:00:00.000Z",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"total_tokens": 10}},
                "rate_limits": {"limit_id": "codex", "primary": null, "secondary": null},
            },
        })],
    );

    let usage = super::read_rollout(&path).expect("rollout should parse");
    assert_eq!(usage.requests, 1);
    assert!(usage.rate_limits.is_none(), "全 null 的窗口不能变成一条读数");
}

/// 已经归属过的会话不能改记到现在这个号头上。
///
/// 上报要跨运行才补得全：一次 `codex exec` 的最后一轮响应写完 rollout 就退出了，
/// 没有后续调用能把它带上去，只能等下一次运行补报——而那时手上很可能是另一个号。
/// 用量的唯一键是 (session_id, account_key)，记错号不是覆盖而是**多出一整行**，
/// 两个号各背一份完整用量。
#[test]
fn a_session_keeps_the_account_that_actually_served_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let day = home.path().join("sessions/2026/08/03");
    std::fs::create_dir_all(&day).expect("session dir");
    write_rollout(&day, &[token_count_event(1_000, 5.0)]);

    let long_ago = std::time::SystemTime::UNIX_EPOCH;
    let first = super::collect_sessions(home.path(), "acct-a", long_ago);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].account_key.as_deref(), Some("acct-a"));

    // 下一次运行手上是另一个号，但这个会话是 acct-a 跑的。
    let again = super::collect_sessions(home.path(), "acct-b", std::time::SystemTime::now());
    assert_eq!(again[0].account_key.as_deref(), Some("acct-a"));
}

/// 归属要在**还没有用量**的时候就落下。
///
/// 一次运行里最早那次扫描发生在第一个响应之前，那时 rollout 只有会话头、没有
/// `token_count`。那时不落归属的话这个会话就永远没有主人，下一次运行也不敢认领
/// ——于是每一轮的最后一次响应都会丢，而只跑一轮的 `codex exec` 等于什么都报不
/// 出来。真机上就是这么坏的：两次运行跑通了请求，库里一行用量都没有。
#[test]
fn a_session_is_claimed_before_it_has_any_usage() {
    let home = tempfile::tempdir().expect("tempdir");
    let day = home.path().join("sessions/2026/08/03");
    std::fs::create_dir_all(&day).expect("session dir");
    // 只有会话头，没有 token_count —— 第一个响应还没回来的样子。
    write_rollout(
        &day,
        &[serde_json::json!({"timestamp": "2026-08-03T00:00:00.000Z",
                             "type": "session_meta", "payload": {}})],
    );
    let long_ago = std::time::SystemTime::UNIX_EPOCH;
    assert!(
        super::collect_sessions(home.path(), "acct-a", long_ago).is_empty(),
        "还没有用量，这一轮不该报任何东西"
    );

    // 响应回来了，rollout 补上 token_count。此刻「本次运行」已经是新的一次了。
    write_rollout(&day, &[token_count_event(1_000, 5.0)]);
    let later = super::collect_sessions(home.path(), "acct-b", std::time::SystemTime::now());
    assert_eq!(later.len(), 1, "上一次运行的尾巴必须补得上来");
    assert_eq!(later[0].account_key.as_deref(), Some("acct-a"));
}

/// 本次运行没碰过、又没有归属记录的会话，一条都不能认领。
///
/// 同一个 `CODEX_HOME` 里躺着装池子之前的历史、以及用户自己 `codex login` 跑的
/// 会话。认领它们等于把一整份用量凭空记到当前这个号头上——真机第一次跑就撞上了，
/// 一个池子建立之前的会话被记了 24601 个 token。
#[test]
fn a_session_from_before_the_pool_is_not_claimed() {
    let home = tempfile::tempdir().expect("tempdir");
    let day = home.path().join("sessions/2026/08/03");
    std::fs::create_dir_all(&day).expect("session dir");
    write_rollout(&day, &[token_count_event(24_601, 5.0)]);

    // 「本次运行」从现在开始，而那个 rollout 是之前写的。
    let claimed = super::collect_sessions(home.path(), "acct-a", std::time::SystemTime::now());
    assert!(claimed.is_empty(), "不认识的历史会话不能算到当前账号头上");
}

/// 归属表不能无限长下去：扫不到的会话再也不会被上报，留着没有意义。
#[test]
fn the_attribution_file_only_keeps_what_is_still_reportable() {
    let home = tempfile::tempdir().expect("tempdir");
    let day = home.path().join("sessions/2026/08/03");
    std::fs::create_dir_all(&day).expect("session dir");
    write_rollout(&day, &[token_count_event(1_000, 5.0)]);
    let long_ago = std::time::SystemTime::UNIX_EPOCH;
    super::collect_sessions(home.path(), "acct-a", long_ago);

    // 会话文件没了，归属记录也该跟着走。
    std::fs::remove_dir_all(&day).expect("drop the rollout");
    super::collect_sessions(home.path(), "acct-a", long_ago);
    let stored = std::fs::read_to_string(home.path().join("pool-sessions.json"))
        .expect("attribution file");
    assert_eq!(stored, "{}");
}

/// 没配就必须返回 None —— 那是「退回上游本地 auth.json」的信号，不是错误。
#[test]
fn an_unconfigured_home_leaves_upstream_auth_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    assert!(Config::load(home.path()).is_none());
}

fn fake_access_token() -> String {
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = b64(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "email": "pool@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_user_id": "user-pool",
            "chatgpt_account_id": "acct-pool",
            "chatgpt_plan_type": "plus",
        },
    });
    let payload = b64(&serde_json::to_vec(&payload).expect("payload"));
    format!("{header}.{payload}.{}", b64(b"sig"))
}

fn lease_body(account_key: &str) -> serde_json::Value {
    serde_json::json!({
        "code": 0,
        "data": {
            "account_key": account_key,
            "plan": "plus",
            "lease_ttl_seconds": 600,
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
    let pool = PoolAuth::new(
        Config {
            base_url: server.uri(),
            key: "test-key".to_string(),
            device_id: "test-device".to_string(),
        },
        home.path().to_path_buf(),
    );
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
