//! The one thing `gea admin` needs that no other group does: a 403 that names `write:admin`.

use gitea_core::error::{Error, ErrorKind};

/// Turn a `403` on an admin route into an error that names `write:admin`.
///
/// Gitea's own refusal for a non-admin token on `/admin/…` is `user must be site admin`, which
/// mentions neither a scope nor a token, so [`gitea_core::error::classify`] correctly reports it
/// as a plain `Forbidden` — correct, but it leaves the reader without the one fact that fixes it:
/// **a Gitea token's scopes are fixed at creation, so `write:admin` means minting a new token.**
/// Every command under `gea admin` therefore passes its errors through here.
///
/// Deliberately narrow: it only *adds* the scope when classification found none, so a 403 that
/// really is about something else (a purge refused because the user still owns repositories)
/// keeps its own message.
pub fn admin_scope(needed: &str, host: &str, settings_url: &str, e: Error) -> Error {
    if !matches!(&*e.kind, ErrorKind::Forbidden { .. }) {
        return e;
    }
    let ctx = (*e.ctx).clone();
    Error::new(ErrorKind::InsufficientScope {
        host: host.to_owned(),
        needed: vec![needed.to_owned()],
        have: None,
        settings_url: settings_url.to_owned(),
    })
    .with_ctx(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: an admin 403 rendering as "the server would not allow this" with no
    /// mention of the scope, which leaves the operator with nothing to do next.
    #[test]
    fn an_admin_403_names_write_admin() {
        let plain = Error::new(ErrorKind::Forbidden {
            server_message: "user must be site admin".to_owned(),
        });
        let mapped =
            admin_scope("write:admin", "forge.test", "https://forge.test/user/settings", plain);
        let ErrorKind::InsufficientScope { needed, .. } = &*mapped.kind else {
            panic!("expected InsufficientScope, got {:?}", mapped.kind)
        };
        assert_eq!(needed, &vec!["write:admin".to_owned()]);
        assert_eq!(mapped.exit_code(), 4);

        // A 403 that classification already understood keeps its own diagnosis.
        let scoped = Error::new(ErrorKind::InsufficientScope {
            host: "h".into(),
            needed: vec!["read:admin".into()],
            have: None,
            settings_url: "u".into(),
        });
        let kept = admin_scope("write:admin", "h", "u", scoped);
        let ErrorKind::InsufficientScope { needed, .. } = &*kept.kind else { panic!() };
        assert_eq!(needed, &vec!["read:admin".to_owned()]);
    }
}
