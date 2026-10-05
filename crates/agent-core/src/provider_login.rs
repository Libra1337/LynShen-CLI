//! Standalone credential login. Does not construct an engine or a session.
use crate::config::{AuthStore, Config};
use llm_provider_kit::auth::{self, LoginContext, LoginOutcome};
use serde_json::{json, Value};

pub fn login(provider: &str, emit: &dyn Fn(Value)) -> Result<(), String> {
    // Only expose flows whose transport and refresh are supported by this build.
    if !matches!(provider, "openai-codex" | "anthropic") {
        return Err("Unsupported browser login provider".into());
    }
    let config = Config::load_existing().map_err(|e| e.to_string())?;
    let outcome = auth::login(
        provider,
        &LoginContext {
            profile_dir: config.profile_dir(),
            client_name: "LynShen Desktop",
        },
        &|url, _| emit(json!({"status":"waiting", "url":url})),
        &|_| {},
        None,
    )?;
    let LoginOutcome::Credentials(credential) = outcome else {
        return Err("Provider requires an API key".into());
    };
    // Reload after the browser round trip so other providers added meanwhile survive.
    let mut auth = AuthStore::load_or_create(config.encrypt_secrets).map_err(|e| e.to_string())?;
    auth.set_oauth_credential(provider, *credential);
    auth.save().map_err(|e| e.to_string())?;
    emit(json!({"status":"complete", "provider":provider}));
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_provider_does_not_open_browser_or_read_config() {
        assert!(super::login("openai", &|_| panic!("unexpected event")).is_err());
    }
}
