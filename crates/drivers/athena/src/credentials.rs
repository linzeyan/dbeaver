//! Where the access key comes from.
//!
//! **No server has answered this driver**, and nothing here needs one: which
//! file is read, in which order, and what a profile in it means are decided on
//! this side and are checked below.
//!
//! **Three places, in this order.** The connection string, then the
//! environment, then `~/.aws/credentials`. That is AWS's own order with the
//! first entry replaced: their chain starts with the environment, because an SDK
//! has no connection string to read. Here there is one, and a key typed into a
//! connection form has to win over one that happens to be exported in the
//! terminal the application was launched from — otherwise a user who fills in
//! the form connects as somebody else and nothing says so.
//!
//! **Two places deliberately not read, and both would cost something.** The
//! instance metadata service answers only inside EC2 and ECS; probing it from a
//! laptop spends a connection timeout on every connect proving the laptop is not
//! a VM. And SSO — `aws sso login` — leaves a cached bearer token that has to be
//! exchanged through `sso:GetRoleCredentials` for a key, which is a second
//! service, a second signature-less call and a token cache with its own expiry
//! rules. Both are worth having and neither is worth guessing at without a way
//! to run it once.
//!
//! **The access key id and the secret go in the connection string's user and
//! password fields**, which is the one place a cloud database fits the shape of
//! this workspace's connection form exactly: they are a name and a secret, the
//! form already has a box for each, and the password box already knows not to
//! show what is typed in it.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::AthenaError;

/// What a signature needs, however it was found.
#[derive(Debug, Clone, Default)]
pub(crate) struct Keys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// The region, where the source that carried the key also carried one. The
    /// connection string's own region wins over this.
    pub region: String,
}

impl Keys {
    fn is_complete(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }
}

/// The profile a credentials file is read under.
///
/// `AWS_PROFILE` and then `default`, which is what every AWS tool does. The
/// section in the file is `[name]`, except that `~/.aws/config` writes
/// `[profile name]` and `~/.aws/credentials` does not — only the second is read
/// here, so only the first spelling is looked for.
pub(crate) fn profile_name(env: &impl Fn(&str) -> Option<String>) -> String {
    env("AWS_PROFILE")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Where the shared credentials file is.
pub(crate) fn credentials_path(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(explicit) = env("AWS_SHARED_CREDENTIALS_FILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let home = env("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".aws").join("credentials"))
}

/// The keys the environment carries, which may be none of them.
pub(crate) fn from_environment(env: &impl Fn(&str) -> Option<String>) -> Keys {
    Keys {
        access_key_id: env("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        secret_access_key: env("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        session_token: env("AWS_SESSION_TOKEN").filter(|t| !t.is_empty()),
        // Both spellings, because both are in use: `AWS_REGION` is what the SDKs
        // read and `AWS_DEFAULT_REGION` is what the CLI has always read, and a
        // shell configured for the CLI is the commonest one there is.
        region: env("AWS_REGION")
            .filter(|r| !r.is_empty())
            .or_else(|| env("AWS_DEFAULT_REGION"))
            .unwrap_or_default(),
    }
}

/// One profile out of a shared credentials file.
///
/// A parser rather than a dependency, because the subset that matters is
/// `[section]` and `key = value` and the file has no nesting, no types and no
/// quoting. What it does have — and what this handles — is comments introduced
/// by `#` or `;`, whitespace around both sides of the `=`, and keys whose case
/// AWS's own tools write inconsistently.
pub(crate) fn from_profile(text: &str, profile: &str) -> Keys {
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // `~/.aws/config` writes `[profile dev]` where `~/.aws/credentials`
            // writes `[dev]`. Only the second file is read here, and accepting
            // the first spelling as well costs one line and makes a user who
            // pointed `AWS_SHARED_CREDENTIALS_FILE` at their config file get
            // what they meant.
            let name = name.trim().strip_prefix("profile ").unwrap_or(name.trim());
            inside = name == profile;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    Keys {
        access_key_id: fields.remove("aws_access_key_id").unwrap_or_default(),
        secret_access_key: fields.remove("aws_secret_access_key").unwrap_or_default(),
        session_token: fields.remove("aws_session_token").filter(|t| !t.is_empty()),
        region: fields.remove("region").unwrap_or_default(),
    }
}

/// The first of the three sources that has a complete key.
///
/// "Complete" is both halves: a source with an id and no secret is a
/// misconfiguration rather than a partial answer, and falling through to the
/// next source would silently connect as somebody else. So a source is taken
/// whole or skipped whole — but its *region* is kept either way, because a
/// profile that names a region and gets its key from the environment is an
/// ordinary arrangement.
pub(crate) fn resolve(
    from_url: Keys,
    env: &impl Fn(&str) -> Option<String>,
    read: impl Fn(&PathBuf) -> Option<String>,
) -> Result<Keys, AthenaError> {
    let environment = from_environment(env);
    let file = credentials_path(env)
        .and_then(|path| read(&path))
        .map(|text| from_profile(&text, &profile_name(env)))
        .unwrap_or_default();

    let region = [&from_url.region, &environment.region, &file.region]
        .into_iter()
        .find(|region| !region.is_empty())
        .cloned()
        .unwrap_or_default();

    for source in [from_url, environment, file] {
        if source.is_complete() {
            return Ok(Keys { region, ..source });
        }
    }
    Err(AthenaError::Credentials(
        "no AWS credentials: put them in the connection string as \
         athena://<key id>:<secret>@<region>/<database>, export AWS_ACCESS_KEY_ID and \
         AWS_SECRET_ACCESS_KEY, or write a profile in ~/.aws/credentials"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// The file shape every AWS tool writes, including the parts that are easy
    /// to skip: comments, whitespace, and a second profile that must not leak
    /// into the first.
    #[test]
    fn a_profile_is_read_and_the_next_one_is_not() {
        let text = "\
# a comment
[default]
aws_access_key_id = AKIADEFAULT
aws_secret_access_key = defaultsecret

; another comment
[dev]
AWS_ACCESS_KEY_ID=AKIADEV
aws_secret_access_key   =   devsecret
aws_session_token = FQoGZXIvYXdz
region = eu-west-1
";
        let default = from_profile(text, "default");
        assert_eq!(default.access_key_id, "AKIADEFAULT");
        assert_eq!(default.secret_access_key, "defaultsecret");
        assert_eq!(default.session_token, None);
        assert_eq!(default.region, "");

        // The key names are upper case here, which AWS's own tools do write.
        let dev = from_profile(text, "dev");
        assert_eq!(dev.access_key_id, "AKIADEV");
        assert_eq!(dev.secret_access_key, "devsecret");
        assert_eq!(dev.session_token.as_deref(), Some("FQoGZXIvYXdz"));
        assert_eq!(dev.region, "eu-west-1");

        assert!(!from_profile(text, "nobody").is_complete());
    }

    /// `~/.aws/config` names its sections differently, and somebody who points
    /// the variable at that file should get what they meant.
    #[test]
    fn a_config_style_section_name_is_understood_too() {
        let text = "[profile dev]\naws_access_key_id = AKIADEV\naws_secret_access_key = s\n";
        assert_eq!(from_profile(text, "dev").access_key_id, "AKIADEV");
    }

    /// The connection string wins, which is the one place this order differs
    /// from AWS's own — a key typed into a form must not be silently replaced by
    /// one exported in the terminal.
    #[test]
    fn the_connection_string_beats_the_environment_and_the_file() {
        let typed = Keys {
            access_key_id: "AKIATYPED".to_string(),
            secret_access_key: "typed".to_string(),
            session_token: None,
            region: "us-east-1".to_string(),
        };
        let keys = resolve(
            typed,
            &|name| match name {
                "AWS_ACCESS_KEY_ID" => Some("AKIAENV".to_string()),
                "AWS_SECRET_ACCESS_KEY" => Some("env".to_string()),
                _ => None,
            },
            |_| None,
        )
        .expect("credentials");
        assert_eq!(keys.access_key_id, "AKIATYPED");
        assert_eq!(keys.region, "us-east-1");
    }

    /// A source with half a key is skipped whole rather than merged with the
    /// next one — half of one key and half of another is a signature that fails
    /// with no explanation.
    #[test]
    fn a_half_filled_source_is_skipped_rather_than_completed_from_the_next() {
        let half = Keys {
            access_key_id: "AKIATYPED".to_string(),
            secret_access_key: String::new(),
            session_token: None,
            region: String::new(),
        };
        let keys = resolve(
            half,
            &|name| match name {
                "AWS_ACCESS_KEY_ID" => Some("AKIAENV".to_string()),
                "AWS_SECRET_ACCESS_KEY" => Some("env".to_string()),
                "AWS_REGION" => Some("eu-west-2".to_string()),
                _ => None,
            },
            |_| None,
        )
        .expect("credentials");
        assert_eq!(keys.access_key_id, "AKIAENV");
        assert_eq!(keys.secret_access_key, "env");
    }

    /// A region named by a profile reaches a key that came from the
    /// environment, which is an ordinary arrangement and would otherwise be a
    /// connection that cannot say which endpoint it wants.
    #[test]
    fn a_region_is_taken_from_whichever_source_has_one() {
        let keys = resolve(
            Keys::default(),
            &|name| match name {
                "AWS_ACCESS_KEY_ID" => Some("AKIAENV".to_string()),
                "AWS_SECRET_ACCESS_KEY" => Some("env".to_string()),
                "HOME" => Some("/Users/somebody".to_string()),
                _ => None,
            },
            |path| {
                assert_eq!(path, &PathBuf::from("/Users/somebody/.aws/credentials"));
                Some("[default]\nregion = ap-northeast-1\n".to_string())
            },
        )
        .expect("credentials");
        assert_eq!(keys.access_key_id, "AKIAENV");
        assert_eq!(keys.region, "ap-northeast-1");
    }

    /// Nothing anywhere says what to do about it, naming all three places.
    #[test]
    fn no_credentials_anywhere_says_where_they_could_go() {
        let error = resolve(Keys::default(), &no_env, |_| None).expect_err("no credentials");
        let message = error.to_string();
        assert!(message.contains("athena://"), "{message}");
        assert!(message.contains("AWS_ACCESS_KEY_ID"), "{message}");
        assert!(message.contains("~/.aws/credentials"), "{message}");
    }

    /// The profile every tool falls back to, and the variable that overrides it.
    #[test]
    fn the_profile_is_the_one_the_environment_names_or_default() {
        assert_eq!(profile_name(&no_env), "default");
        assert_eq!(
            profile_name(&|name| (name == "AWS_PROFILE").then(|| "dev".to_string())),
            "dev"
        );
    }
}
