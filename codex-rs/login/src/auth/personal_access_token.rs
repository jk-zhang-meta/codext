use codex_http_client::HttpClient;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::PlanType as InternalPlanType;
use serde::Deserialize;
use std::env;
use std::fmt;

use crate::default_client::create_default_auth_client;
use crate::outbound_proxy::AuthRouteConfig;

const PROD_AUTHAPI_BASE_URL: &str = "https://auth.openai.com/api/accounts";
const CODEX_AUTHAPI_BASE_URL_ENV_VAR: &str = "CODEX_AUTHAPI_BASE_URL";
const WHOAMI_PATH: &str = "/v1/user-auth-credential/whoami";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct PersonalAccessTokenMetadata {
    email: Option<String>,
    chatgpt_user_id: String,
    chatgpt_account_id: String,
    chatgpt_plan_type: String,
    chatgpt_account_is_fedramp: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PersonalAccessTokenAuth {
    access_token: String,
    metadata: PersonalAccessTokenMetadata,
}

impl fmt::Debug for PersonalAccessTokenAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersonalAccessTokenAuth")
            .field("access_token", &"<redacted>")
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl PersonalAccessTokenAuth {
    pub(super) async fn load(
        access_token: &str,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<Self> {
        let authapi_base_url = env::var(CODEX_AUTHAPI_BASE_URL_ENV_VAR)
            .ok()
            .map(|base_url| base_url.trim().trim_end_matches('/').to_string())
            .filter(|base_url| !base_url.is_empty())
            .unwrap_or_else(|| PROD_AUTHAPI_BASE_URL.to_string());
        let endpoint = whoami_endpoint(&authapi_base_url);
        let client = create_default_auth_client(&endpoint, auth_route_config)?;
        hydrate_personal_access_token(&client, &endpoint, access_token).await
    }

    /// 用**别人已经查好的**身份组装一份 PAT 凭据，不再自己去 whoami 问一趟。
    ///
    /// 号池就是这个"别人"：服务端派号时已经把 account_id、套餐、以及形如
    /// `user-…::acct-…` 的 account_key 一起发下来了，那些正是 whoami 会回的
    /// 东西。再问一次既多一个网络往返，也多一个会失败的地方——而它一旦失败，
    /// 一份本来能用的凭据就整份作废。
    ///
    /// 拿不到的字段老实留空（email），不编。fedramp 一律按 false：这是个
    /// 需要明确证据才能置位的标志，猜错会把请求发去错误的边缘节点。
    pub(super) fn from_external(
        access_token: &str,
        chatgpt_user_id: &str,
        chatgpt_account_id: &str,
        chatgpt_plan_type: Option<&str>,
    ) -> Self {
        Self {
            access_token: access_token.to_string(),
            metadata: PersonalAccessTokenMetadata {
                email: None,
                chatgpt_user_id: chatgpt_user_id.to_string(),
                chatgpt_account_id: chatgpt_account_id.to_string(),
                chatgpt_plan_type: chatgpt_plan_type.unwrap_or("unknown").to_string(),
                chatgpt_account_is_fedramp: false,
            },
        }
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn account_id(&self) -> &str {
        &self.metadata.chatgpt_account_id
    }

    pub fn chatgpt_user_id(&self) -> &str {
        &self.metadata.chatgpt_user_id
    }

    pub fn email(&self) -> Option<&str> {
        self.metadata.email.as_deref()
    }

    pub fn plan_type(&self) -> AccountPlanType {
        InternalPlanType::from_raw_value(&self.metadata.chatgpt_plan_type).into()
    }

    pub fn is_fedramp_account(&self) -> bool {
        self.metadata.chatgpt_account_is_fedramp
    }
}

async fn hydrate_personal_access_token(
    client: &HttpClient,
    endpoint: &str,
    access_token: &str,
) -> std::io::Result<PersonalAccessTokenAuth> {
    let response = client
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|err| {
            std::io::Error::other(format!(
                "failed to request personal access token metadata: {err}"
            ))
        })?;
    if !response.status().is_success() {
        return Err(std::io::Error::other(format!(
            "personal access token metadata request failed with status {}",
            response.status()
        )));
    }

    let metadata = response
        .json::<PersonalAccessTokenMetadata>()
        .await
        .map_err(|err| {
            std::io::Error::other(format!(
                "failed to decode personal access token metadata: {err}"
            ))
        })?;
    Ok(PersonalAccessTokenAuth {
        access_token: access_token.to_string(),
        metadata,
    })
}

fn whoami_endpoint(authapi_base_url: &str) -> String {
    format!("{}{WHOAMI_PATH}", authapi_base_url.trim_end_matches('/'))
}

#[cfg(test)]
#[path = "personal_access_token_tests.rs"]
mod tests;
