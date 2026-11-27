//! `--paginate` for layer 1, page by page.
//!
//! The termination rules are **not** reimplemented here: [`Paginator`] is the one
//! implementation, and it is the one place the clamped-page data-loss bug is guarded against
//! (a server that silently clamps `limit=100` to 50 makes a full page look like the last one).
//! This module only supplies the plumbing [`gitea_core::http::Client`] does not expose
//! publicly — a page-at-a-time walk that keeps page *boundaries*, which `--slurp` needs and
//! which `Client::items` deliberately flattens away.
//!
//! One deviation is worth naming. [`Next::Url`] asks for the server's `Link: rel="next"` to be
//! followed verbatim, and `Client` has no public exit that sends to an absolute URL, so this
//! walk advances by page number instead. Gitea's next links are exactly `?page=N&limit=M`, so
//! for this API the two are the same request; the difference would only matter for a
//! cursor-style link, which the API does not emit. The alternative — hand-rolling a second
//! pagination loop with its own termination rules — is the failure mode that actually loses
//! data.

use gitea_core::error::Result;
use gitea_core::http::paginate::{self, Next, PageInfo, Paginator, StopReason};
use gitea_core::http::{Client, Request};
use serde_json::Value;

/// Every page of `base`, each as a JSON array, plus why the walk stopped.
pub struct Pages {
    pub pages: Vec<Value>,
    pub items: usize,
    pub stopped: StopReason,
}

/// Walk `base` to the last page.
///
/// `limit` is the user's `--limit`: a cap on **total items across all pages**, never a per-page
/// size. Conflating the two is the root of the clamped-page bug, which is why they are separate
/// parameters here and separate fields on [`Paginator`].
pub async fn walk(client: &Client, base: &Request, limit: Option<usize>) -> Result<Pages> {
    // Ask the instance how large a page it will actually honour. When `/settings/api` does not
    // answer, send nothing and let the server choose — guessing a limit we cannot validate
    // manufactures the very short-page ambiguity we are trying to survive.
    let per_page = if base.has_query("limit") {
        None
    } else {
        let caps = client.capabilities().await.ok();
        paginate::effective_limit(None, caps.as_deref())
    };

    let mut paginator = Paginator::new(limit);
    let mut pages = Vec::new();
    let mut items = 0usize;
    let mut page = 1u32;

    let stopped = loop {
        let mut req = base.clone();
        if let Some(n) = per_page {
            req.set_query("limit", n);
        }
        if page > 1 {
            req.set_query("page", page);
        }

        let (values, info): (Vec<Value>, PageInfo) = client.page(req).await?;
        let keep = paginator.keep(values.len());
        items += keep;
        // `into_iter`, not `iter().cloned()`: `values` is dead after this line — only `info` is
        // read below — so cloning deep-copies every retained JSON tree to drop the original.
        pages.push(Value::Array(values.into_iter().take(keep).collect()));

        match paginator.advance(&info)? {
            Next::Stop(reason) => break reason,
            Next::Url(_) | Next::Page(_) => page = paginator.pages_seen + 1,
        }
    };

    Ok(Pages { pages, items, stopped })
}

/// Every item from every page, as one array. What `--paginate` without `--slurp` conceptually
/// produces, for callers that want the whole collection rather than the pages.
pub fn flatten(pages: Vec<Value>) -> Value {
    let mut all = Vec::new();
    for page in pages {
        match page {
            Value::Array(items) => all.extend(items),
            other => all.push(other),
        }
    }
    Value::Array(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use gitea_core::http::{Auth, Client};
    use std::sync::Arc;

    /// `http::Method` is not a direct dependency of this crate, so its constants cannot be
    /// named. `FakeTransport::on` wants one, and the type is inferrable from that parameter.
    fn method<T: std::str::FromStr>(name: &str) -> T {
        name.parse().unwrap_or_else(|_| panic!("{name} is a literal method name"))
    }

    fn page_of(n: usize, from: usize) -> String {
        let items: Vec<String> = (from..from + n).map(|i| format!("{{\"id\":{i}}}")).collect();
        format!("[{}]", items.join(","))
    }

    /// The highest-consequence silent bug in the tool, at layer 1: Gitea clamps the requested
    /// `limit` to `max_response_items`, so a page shorter than what we asked for is **not**
    /// evidence that it is the last page. Terminating on that would drop everything past the
    /// clamp. Here the server clamps 100 to 50 across three pages and we must see all 120
    /// items.
    #[tokio::test]
    async fn a_server_that_clamps_the_page_size_does_not_truncate_the_walk() {
        let t = Arc::new(
            FakeTransport::new()
                .on_sequence(
                    method("GET"),
                    "/api/v1/repos/o/r/issues",
                    vec![
                        Canned::json(200, page_of(50, 0)),
                        Canned::json(200, page_of(50, 50)),
                        Canned::json(200, page_of(20, 100)),
                    ],
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            .build()
            .unwrap();

        let out = walk(&client, &Request::get("/repos/o/r/issues"), None).await.unwrap();
        assert_eq!(out.items, 120, "items past the clamp were dropped");
        assert_eq!(out.pages.len(), 3, "page boundaries must survive for --slurp");
        assert_eq!(out.stopped, StopReason::ShortPageWithoutLink);
        assert_eq!(flatten(out.pages).as_array().map(Vec::len), Some(120));
    }

    #[tokio::test]
    async fn a_user_limit_caps_the_total_not_the_page() {
        let t = Arc::new(
            FakeTransport::new()
                .on_sequence(
                    method("GET"),
                    "/api/v1/repos/o/r/issues",
                    vec![Canned::json(200, page_of(50, 0)), Canned::json(200, page_of(50, 50))],
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            .build()
            .unwrap();
        let out = walk(&client, &Request::get("/repos/o/r/issues"), Some(70)).await.unwrap();
        assert_eq!(out.items, 70);
        assert_eq!(out.stopped, StopReason::UserLimit);
        assert_eq!(
            out.pages.iter().map(|p| p.as_array().unwrap().len()).collect::<Vec<_>>(),
            [50, 20]
        );
    }

    /// A `Link` header with no `next` is authoritative: one page, one request.
    #[tokio::test]
    async fn a_link_header_without_a_next_stops_after_one_request() {
        let t = Arc::new(FakeTransport::new().on(
            method("GET"),
            "/api/v1/repos/o/r/issues",
            Canned::json(200, page_of(30, 0)).with_header(
                "link",
                "<https://git.example.org/api/v1/repos/o/r/issues?page=1>; rel=\"first\"",
            ),
        ));
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            .build()
            .unwrap();
        let out = walk(&client, &Request::get("/repos/o/r/issues"), None).await.unwrap();
        assert_eq!(out.stopped, StopReason::LinkExhausted);
        assert_eq!(out.pages.len(), 1);
    }
}
