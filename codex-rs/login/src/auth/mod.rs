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

mod external_bearer;
mod manager;
mod revoke;

pub use auth_headers::AuthHeaders;
pub use bedrock_api_key::BedrockApiKeyAuth;
pub use bedrock_api_key::login_with_bedrock_api_key;
// codext: 池子的枯竭状态要能被 core 的重试循环看到——"没号可发"必须显性地报给
// 用户，而不是悄悄退回本地号或者把会话打断。
pub use pool::pool_is_exhausted;
pub use pool::take_pool_exhaustion_notice;

pub use error::RefreshTokenFailedError;
pub use error::RefreshTokenFailedReason;
pub use manager::*;
