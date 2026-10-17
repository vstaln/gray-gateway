use std::collections::BTreeSet;

pub const REDACTION_ABSOLUTE_PATH: &str = "absolute_path";
pub const REDACTION_RELATIVE_PATH: &str = "relative_path";
pub const REDACTION_SECRET: &str = "secret";

const PATH_PLACEHOLDER: &str = "<path>";

pub const REDACTED: &str = "<redacted>";

const SECRET_NAME_MARKERS: &[&str] = &[
    "api_key",
    "apikey",
    "secret",
    "token",
    "password",
    "passwd",
    "credential",
    "authorization",
    "auth_token",
    "access_key",
    "private_key",
    "session_key",
];

const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xapp-",
];

const AUTH_SCHEMES: &[&str] = &["Bearer", "Basic", "Digest", "Token"];

const SELF_EVIDENT_AUTH_SCHEME: &str = "Bearer";

const AWS_KEY_ID_PREFIXES: &[&str] = &[
    "AKIA", "ASIA", "AGPA", "AIDA", "AROA", "ANPA", "ANVA", "ASCA", "ABIA", "ACCA",
];

const AWS_KEY_ID_LEN: usize = 20;

const CREDENTIAL_VALUE_MIN_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    text: String,
    kinds: BTreeSet<String>,
}

impl Redaction {
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    #[must_use]
    pub fn has_secret(&self) -> bool {
        self.kinds.iter().any(|k| k == REDACTION_SECRET)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CredentialArm {
    None,

    IfShaped,

    Certain,
}

impl CredentialArm {
    const fn is_armed(self) -> bool {
        !matches!(self, Self::None)
    }
}

struct TokenOutcome {
    text: Option<String>,

    arm: CredentialArm,
}

impl TokenOutcome {
    const fn keep() -> Self {
        Self {
            text: None,
            arm: CredentialArm::None,
        }
    }

    const fn keep_and_arm(arm: CredentialArm) -> Self {
        Self { text: None, arm }
    }

    fn replace(text: String) -> Self {
        Self {
            text: Some(text),
            arm: CredentialArm::None,
        }
    }
}

#[must_use]
pub fn redact_for_disclosure(input: &str) -> Redaction {
    if input.contains('\n') {
        let mut kinds = BTreeSet::new();
        let text = input
            .split('\n')
            .map(|line| {
                let redaction = redact_for_disclosure(line);
                kinds.extend(redaction.kinds.iter().cloned());
                redaction.into_text()
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Redaction { text, kinds };
    }
    let mut kinds = BTreeSet::new();
    let tokens: Vec<&str> = input.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut arm = CredentialArm::None;

    for (index, token) in tokens.iter().enumerate() {
        if token.is_empty() {
            out.push(String::new());
            continue;
        }
        if arm.is_armed() {
            if is_auth_scheme(token) {
                if is_canonical_auth_scheme(token) {
                    arm = CredentialArm::Certain;
                }
                out.push((*token).to_string());
                continue;
            }

            if arm == CredentialArm::Certain || looks_like_credential_value(token) {
                kinds.insert(REDACTION_SECRET.to_string());
                out.push(REDACTED.to_string());
                arm = CredentialArm::None;
                continue;
            }
        }

        let next = tokens[index + 1..]
            .iter()
            .copied()
            .find(|candidate| !candidate.is_empty());
        let outcome = redact_token(token, next, &mut kinds);
        arm = outcome.arm;
        out.push(outcome.text.unwrap_or_else(|| (*token).to_string()));
    }

    Redaction {
        text: out.join(" "),
        kinds,
    }
}

fn redact_token(token: &str, next: Option<&str>, kinds: &mut BTreeSet<String>) -> TokenOutcome {
    for separator in ['=', ':'] {
        let Some((name, value)) = token.split_once(separator) else {
            continue;
        };
        let lowered = name.to_ascii_lowercase();
        let secret_name = SECRET_NAME_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker));
        if secret_name {
            if is_auth_scheme(value) {
                return TokenOutcome::keep_and_arm(if is_canonical_auth_scheme(value) {
                    CredentialArm::Certain
                } else {
                    CredentialArm::IfShaped
                });
            }

            if value.is_empty() {
                return if next
                    .is_some_and(|next| is_auth_scheme(next) || looks_like_credential_value(next))
                {
                    TokenOutcome::keep_and_arm(CredentialArm::IfShaped)
                } else {
                    TokenOutcome::keep()
                };
            }
            kinds.insert(REDACTION_SECRET.to_string());
            return TokenOutcome::replace(format!("{name}{separator}{REDACTED}"));
        }

        if let Some(kind) = classify_path(value) {
            kinds.insert(kind.to_string());
            return TokenOutcome::replace(format!("{name}{separator}{PATH_PLACEHOLDER}"));
        }
    }

    if unwrap_token(token) == SELF_EVIDENT_AUTH_SCHEME {
        return TokenOutcome::keep_and_arm(CredentialArm::Certain);
    }
    if is_auth_scheme(token) && next.is_some_and(looks_like_credential_value) {
        return TokenOutcome::keep_and_arm(CredentialArm::IfShaped);
    }

    let lowered = unwrap_token(token).to_ascii_lowercase();
    let value_lowered = token
        .split_once(['=', ':'])
        .map(|(_, value)| unwrap_token(value).to_ascii_lowercase())
        .unwrap_or_default();
    if SECRET_VALUE_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix) || value_lowered.starts_with(prefix))
        || looks_like_aws_access_key(token)
    {
        kinds.insert(REDACTION_SECRET.to_string());
        return TokenOutcome::replace(REDACTED.to_string());
    }

    if let Some(kind) = classify_path(token) {
        kinds.insert(kind.to_string());
        return TokenOutcome::replace(PATH_PLACEHOLDER.to_string());
    }

    TokenOutcome::keep()
}

fn unwrap_token(token: &str) -> &str {
    token
        .trim_start_matches(['(', '[', '"', '\'', '<', '`', '*'])
        .trim_end_matches([
            ',', '.', ';', ':', ')', ']', '"', '\'', '>', '!', '?', '`', '*',
        ])
}

fn is_auth_scheme(token: &str) -> bool {
    let word = unwrap_token(token);
    !word.is_empty()
        && word.chars().all(|ch| ch.is_ascii_alphabetic())
        && AUTH_SCHEMES
            .iter()
            .any(|scheme| scheme.eq_ignore_ascii_case(word))
}

fn is_canonical_auth_scheme(token: &str) -> bool {
    AUTH_SCHEMES.contains(&unwrap_token(token))
}

fn looks_like_credential_value(token: &str) -> bool {
    let value = unwrap_token(token);
    if value.chars().count() < CREDENTIAL_VALUE_MIN_LEN {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            has_digit = true;
        } else if ch.is_ascii_alphabetic() {
            has_alpha = true;
        } else if !matches!(ch, '-' | '_' | '.' | '=' | '+' | '/' | '~') {
            return false;
        }
    }
    has_digit && has_alpha
}

fn looks_like_aws_access_key(token: &str) -> bool {
    let value = unwrap_token(token);
    if value.len() < AWS_KEY_ID_LEN {
        return false;
    }
    if !AWS_KEY_ID_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
    {
        return false;
    }
    value
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn looks_absolute(token: &str) -> bool {
    let trimmed = token.trim_start_matches(['(', '[', '"', '\'']);
    if trimmed.len() < 2 {
        return false;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        return rest.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_');
    }
    if trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
        return true;
    }
    if trimmed.starts_with("\\\\") {
        return true;
    }
    let mut chars = trimmed.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    )
}

fn classify_path(token: &str) -> Option<&'static str> {
    let raw = trim_path_punctuation(token);
    let unescaped = unescape_path(raw);
    let candidate = trim_path_punctuation(&unescaped);
    if candidate.contains("://") {
        return None;
    }

    if looks_absolute(raw) || looks_absolute(candidate) {
        return Some(REDACTION_ABSOLUTE_PATH);
    }
    if looks_relative(candidate) {
        return Some(REDACTION_RELATIVE_PATH);
    }
    None
}

fn trim_path_punctuation(token: &str) -> &str {
    token
        .trim_start_matches(['(', '[', '{', '"', '\'', '<', '`'])
        .trim_end_matches([
            ',', ';', ':', '.', ')', ']', '}', '"', '\'', '>', '`', '!', '?',
        ])
}

fn unescape_path(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('/') => {
                out.push('/');
                chars.next();
            }
            Some('\\') => {
                out.push('\\');
                chars.next();
            }
            Some('"') | Some('\'') => {
                chars.next();
            }

            _ => out.push('\\'),
        }
    }
    out
}

fn looks_relative(candidate: &str) -> bool {
    if !candidate.contains(['/', '\\']) {
        return false;
    }
    let explicit_prefix = ["./", "../", ".\\", "..\\"]
        .iter()
        .any(|prefix| candidate.starts_with(prefix));
    if explicit_prefix {
        return true;
    }
    let trimmed = candidate.trim_end_matches(['/', '\\']);
    let segments: Vec<&str> = trimmed.split(['/', '\\']).collect();
    if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
        return false;
    }
    let last = segments[segments.len() - 1];
    let Some((stem, extension)) = last.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=8).contains(&extension.chars().count())
        && extension.chars().all(|ch| ch.is_ascii_alphabetic())
}
