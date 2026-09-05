mod access_token;
mod agent_identity;
mod auth_headers;
mod bedrock_access_keys;
mod bedrock_api_key;
pub mod default_client;
pub mod error;
mod personal_access_token;
mod pool;
mod storage;
mod util;
mod workload_identity;

mod external_bearer;
mod manager;
mod revoke;

pub use auth_headers::AuthHeaders;
pub use bedrock_access_keys::BedrockAccessKeysAuth;
pub use bedrock_access_keys::login_with_bedrock_access_keys;
pub use bedrock_api_key::BedrockApiKeyAuth;
pub use bedrock_api_key::login_with_bedrock_api_key;
// codext: 池子的枯竭状态要能被 core 的重试循环看到——"没号可发"必须显性地报给
// 用户，而不是悄悄退回本地号或者把会话打断。
pub use pool::held_account_email;
pub use pool::pool_is_exhausted;
pub use pool::take_pool_exhaustion_notice;
// codext: 用量在一次模型调用结束那一刻记账，被拒也在发生那一刻上报——只有 core
// 到得了那两个位置，而只有这里知道当时手上是哪个号。
pub use pool::record_turn_usage;
pub use pool::set_session_cwd;
pub use pool::REJECT_UNAUTHORIZED;
pub use pool::REJECT_USAGE_LIMIT;
pub use pool::REJECT_MODEL_NOT_SUPPORTED;
pub use pool::report_account_refused;
// codext: 退出时交回调度名额。只有 `cli` 的 `main` 到得了"这个进程要结束了"这个
// 位置，见 `pool::release_on_exit` 上面那段。
pub use pool::release_on_exit;

pub use error::RefreshTokenFailedError;
pub use error::RefreshTokenFailedReason;
pub use manager::*;
pub use workload_identity::is_workload_identity_selected;
