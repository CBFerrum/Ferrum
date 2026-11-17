//! The two `/app/…` addresses still given out: `/app/login`, the single sign-on return address providers have
//! registered (auth.rs builds it), and `/app/e/{entity}[/{id}]`, which the interface's administration pages and
//! Studio's dashboard links use. Everything else under `/app` goes to `/`.

use axum::extract::Request;
use axum::http::Uri;
use axum::response::{IntoResponse, Redirect, Response};
use loco_rs::controller::Routes;

/// Where an address under `/app` (its path after `/app`, as sent) leads. Segments keep their percent-encoding;
/// every result is a path on this host.
pub fn new_home(path: &str) -> String {
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match seg.as_slice() {
        ["login"] => "/login".into(),
        ["e", entity] => format!("/open/{entity}"),
        ["e", entity, id] => format!("/open/{entity}/{id}"),
        _ => "/".into(),
    }
}

async fn moved(req: Request) -> Response {
    answer(req.uri())
}

/// The answer to an address under `/app` (the shell's catch-all sends `/app/` here: the router
/// takes the slash off this module's routes).
pub fn answer(uri: &Uri) -> Response {
    let path = uri.path().strip_prefix("/app").unwrap_or_default();
    // A provider's answer to signing in again (a step-up) comes to the return address registered with it, as a
    // sign-in's does: its state says which it is.
    if path == "/login"
        && let Some(back) = super::auth::step_up_return(uri.query())
    {
        return Redirect::to(&back).into_response();
    }
    let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    Redirect::to(&format!("{}{q}", new_home(path))).into_response()
}

/// What load balancers and monitors ask (`/healthz`, deploy/smoke.sh).
async fn healthz() -> &'static str {
    "ok\n"
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/app", get(moved)).add("/app/{*rest}", get(moved)).add("/healthz", get(healthz))
}

#[cfg(test)]
mod tests {
    use super::new_home;

    #[test]
    fn the_addresses_still_given_out_lead_to_their_pages() {
        for (old, new) in [
            ("", "/"),
            ("/", "/"),
            ("/login", "/login"),
            ("/e/SalesInvoice", "/open/SalesInvoice"),
            ("/e/Sales%20Invoice/01a1", "/open/Sales%20Invoice/01a1"),
            ("/e/SalesInvoice/01a1/extra", "/"),
            ("/admin/users", "/"),
            ("//evil.example/x", "/"),
            ("/nothing", "/"),
        ] {
            assert_eq!(new_home(old), new, "{old}");
        }
    }

    #[test]
    fn a_step_up_answer_goes_on_to_be_checked_as_one_and_a_sign_in_answer_to_the_sign_in_page() {
        use crate::controllers::auth::step_up_return;
        let to = |uri: &str| super::answer(&uri.parse().unwrap()).headers().get("location").unwrap().to_str().unwrap().to_string();
        assert_eq!(to("/app/login?code=c1&state=su.AbC-_9"), "/api/auth/step-up/provider/back?code=c1&state=su.AbC-_9");
        assert_eq!(to("/app/login?code=c1&state=AbC-_9"), "/login?code=c1&state=AbC-_9", "a sign-in's");
        assert_eq!(to("/app/settings?state=su.x"), "/?state=su.x", "only the return address");
        assert_eq!(
            step_up_return(Some("state=su%2Ex&code=c")).as_deref(),
            Some("/api/auth/step-up/provider/back?state=su%2Ex&code=c"),
            "as the provider encoded it"
        );
        assert_eq!(
            step_up_return(Some("error=access_denied&state=su.x")).as_deref(),
            Some("/api/auth/step-up/provider/back?error=access_denied&state=su.x"),
            "a refusal too"
        );
        assert_eq!(step_up_return(Some("code=su.x&nstate=su.y")), None, "the state alone says so");
        assert_eq!(step_up_return(None), None);
    }
}
