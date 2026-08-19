mod access_token;
mod agent_identity;
mod auth_headers;
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
pub use pool::report_account_refused;

pub use error::RefreshTokenFailedError;
pub use error::RefreshTokenFailedReason;
pub use manager::*;
pub use workload_identity::is_workload_identity_selected;
