//! Signing in to a dashboard served beyond this machine (`orx up --host
//! 0.0.0.0`, reached over Tailscale or the LAN). Requests from this machine
//! need nothing; any other device presents the access token, once through the
//! sign-in link and afterwards through the cookie that link sets.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Context, Result};

pub const COOKIE: &str = "orx_access";
/// The query parameter of the sign-in link.
pub const QUERY: &str = "access";
const FILE: &str = "board-access-token";

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// The access token, created on first use with permissions only the user can read.
pub fn load_or_create(data_dir: &Path) -> Result<String> {
    if let Some(token) = load(data_dir)? {
        return Ok(token);
    }
    rotate(data_dir)
}

pub fn load(data_dir: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(token_path(data_dir)) {
        Ok(text) => Ok(Some(text.trim().to_string()).filter(|token| !token.is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading the board access token"),
    }
}

/// Replaces the token, so every device signed in with the old one is signed out.
pub fn rotate(data_dir: &Path) -> Result<String> {
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let path = token_path(data_dir);
    std::fs::create_dir_all(data_dir)?;
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, &token).context("writing the board access token")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp, &path)?;
    Ok(token)
}

/// What a request from another device gets.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow,
    /// The sign-in link: set the cookie and go to `location`, the same page without the token.
    SignIn {
        location: String,
        cookie: String,
    },
    Deny,
}

/// `cookie` is the request's Cookie header, `bearer` its Authorization bearer,
/// `path_and_query` its target.
pub fn check(
    token: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
    path_and_query: &str,
) -> Verdict {
    let matches =
        |given: &str| Sha256::digest(given.as_bytes()) == Sha256::digest(token.as_bytes());
    let from_cookie = cookie.is_some_and(|header| {
        header
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name == COOKIE && matches(value))
    });
    if from_cookie || bearer.is_some_and(matches) {
        return Verdict::Allow;
    }
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let mut signed = false;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| match pair.split_once('=') {
            Some((name, value)) if name == QUERY => {
                signed |= matches(value);
                false
            }
            _ => true,
        })
        .collect();
    if !signed {
        return Verdict::Deny;
    }
    let location = if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    };
    Verdict::SignIn {
        location,
        cookie: format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"),
    }
}

/// The page another device sees before it has signed in. It names no project.
pub const SIGN_IN_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>OpenResearch</title></head>\
<body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem;line-height:1.5\">\
<h1 style=\"font-size:1.25rem\">Sign in to this dashboard</h1>\
<p>This dashboard only opens on other devices with its access link. On the workstation, run \
<code>orx board token</code> and open the link it prints on this device.</p></body></html>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_or_a_bearer_lets_a_device_in() {
        assert_eq!(
            check("t0k", Some("a=1; orx_access=t0k"), None, "/"),
            Verdict::Allow
        );
        assert_eq!(
            check("t0k", None, Some("t0k"), "/api/board/inbox"),
            Verdict::Allow
        );
        assert_eq!(
            check("t0k", Some("orx_access=old"), Some("nope"), "/"),
            Verdict::Deny
        );
        assert_eq!(check("t0k", None, None, "/projects/p/board"), Verdict::Deny);
    }

    #[test]
    fn the_sign_in_link_sets_the_cookie_and_drops_the_token_from_the_address() {
        let Verdict::SignIn { location, cookie } =
            check("t0k", None, None, "/projects/p/board?access=t0k&tab=inbox")
        else {
            panic!("the link signs in");
        };
        assert_eq!(location, "/projects/p/board?tab=inbox");
        assert!(
            cookie.starts_with("orx_access=t0k;") && cookie.contains("HttpOnly"),
            "{cookie}"
        );
        assert!(
            matches!(check("t0k", None, None, "/?access=t0k"), Verdict::SignIn { location, .. } if location == "/")
        );
        assert_eq!(check("t0k", None, None, "/?access=wrong"), Verdict::Deny);
    }

    #[test]
    fn the_token_file_is_kept_and_rotated() {
        let dir = std::env::temp_dir().join(format!("orx-access-{}", uuid::Uuid::new_v4()));
        let first = load_or_create(&dir).unwrap();
        assert_eq!(load_or_create(&dir).unwrap(), first);
        let second = rotate(&dir).unwrap();
        assert_ne!(first, second);
        assert_eq!(load(&dir).unwrap().as_deref(), Some(second.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
