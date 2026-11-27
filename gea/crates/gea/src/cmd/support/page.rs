//! Limits, draining, and the `Showing N of M` banner.

use futures::StreamExt;
use gitea_core::error::Result;
use gitea_core::http::{ItemStream, PageInfo, Paging};

use crate::global::GlobalOpts;

/// `-L/--limit`'s default, from `docs/porcelain-conventions.md`.
pub const DEFAULT_LIMIT: usize = 30;

/// `-L N` (subcommand) wins over `--limit N` (global) wins over 30.
///
/// `-L` is declared short-only on each list command: the global `--limit` already claims the
/// long name, and clap answers a duplicate long name with a **panic**. So both spellings work
/// and mean the same thing, which is what the conventions ask for.
///
/// Clamped to at least 1: `-L 0` is not a request anyone means.
pub fn limit(local: Option<usize>, globals: &GlobalOpts) -> usize {
    local.or(globals.limit).unwrap_or(DEFAULT_LIMIT).max(1)
}

/// How many items a list command should fetch, for the `--paginate`-aware commands.
///
/// `--limit` is a cap on **total items across all pages**, and without `--paginate` a list
/// command deliberately stops at one page's worth. `None` for `--paginate` with no `--limit` is
/// the "walk it all" case.
pub fn item_cap(globals: &GlobalOpts) -> Option<usize> {
    if globals.paginate { globals.limit } else { Some(globals.limit.unwrap_or(DEFAULT_LIMIT)) }
}

/// Drain an [`ItemStream`] under a cap, for the `--paginate` half of a list command.
///
/// `take` rather than letting the stream's own `Paging` do it: the generated stream methods take
/// no `Paging`, and re-plumbing one through 506 signatures to save a `take` would be a poor
/// trade.
pub async fn drain<T>(stream: ItemStream<T>, cap: Option<usize>) -> Result<Vec<T>> {
    let mut stream = std::pin::pin!(stream.take(cap.unwrap_or(usize::MAX)));
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item?);
    }
    Ok(out)
}

/// `Showing 2 of 40 webhooks`, or `Showing 2 webhooks` when the server did not send
/// `x-total-count`.
///
/// Claiming a total we were not given would be worse than omitting it: the Gitea API sends
/// `x-total-count` on some collections and not others, and `Showing 2 of 2` on a truncated list
/// is a lie a user would act on.
///
/// **This always announces the count**; [`truncation_banner`] announces only when rows were left
/// out. Both spellings exist in the tool and this module is where the difference is visible
/// instead of being an accident of which group you are in — see that function's docs.
pub fn banner(shown: usize, total: Option<u64>, noun: &str) -> String {
    match total {
        Some(t) if t as usize > shown => format!("Showing {shown} of {t} {noun}"),
        _ => format!("Showing {shown} {noun}"),
    }
}

/// `Showing 30 of 94 pull requests`, or nothing at all when the list is complete.
///
/// The other half of the banner question, and a deliberate divergence rather than an oversight:
/// `pr`, `repo` and `run` treat `Showing 30 of 30` as noise and print nothing, while `admin`,
/// `webhook`, `times` and the rest always state the count. Unifying the two is a product
/// decision about how chatty the tool is, not a refactor, so both live here under names that
/// say which is which.
pub fn truncation_banner(shown: usize, total: Option<usize>, noun: &str) -> Option<String> {
    match total {
        Some(t) if t > shown => Some(format!("Showing {shown} of {t} {noun}")),
        _ => None,
    }
}

/// The collection's real size, for a list that was drained from a capped [`ItemStream`].
///
/// A capped stream cannot say how many rows it withheld. It stops at the cap, and the
/// `x-total-count` its pages carried was consumed by the paginator and dropped — `ItemStream` is
/// a bare `Stream` with nowhere to hang a total. So when the list came back *full*, meaning there
/// may well be more behind it, the count is asked for directly: the operation's `_page` twin, one
/// item wide, purely for its header. `probe` is that call with the [`Paging`] supplied.
///
/// Three properties are deliberate:
///
/// - **It costs a request only when the list was truncated.** A list shorter than its cap is the
///   whole collection and already knows its own size, which is the answer returned without asking
///   anybody. `--paginate` with no `--limit` lands here too, because its cap is `usize::MAX`.
/// - **It never fails the listing.** A probe that is refused, 404s, or comes back without the
///   header yields `None`, and the banner falls back to `Showing N` — the wording it had before.
///   Turning a successful list into an error over a cosmetic header would be a poor trade.
/// - **Machine output never reaches it.** `--json`/`--jq`/`--template` return before the banner
///   is built, so no request is spent on a line nobody prints.
///
/// Note this is *not* the shape `block`/`reaction` use. Those fetch the list itself through the
/// `_page` twin, which reads `x-total-count` for free but takes `limit` as a page size — and
/// Gitea silently clamps that to `max_response_items`, so `-L 100` on a 50-item instance
/// quietly returns 50. The lists here follow the `Link` header through `ItemStream` precisely to
/// avoid that, and keeping the total on a separate probe is what lets them keep doing so.
pub async fn total_if_truncated<T, F, Fut>(shown: usize, cap: usize, probe: F) -> Option<u64>
where
    F: FnOnce(Paging) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<T>, PageInfo)>>,
{
    if shown < cap {
        return Some(shown as u64);
    }
    probe(Paging::limit(1)).await.ok().and_then(|(_, info)| info.total_count)
}

/// `"s"` unless there is exactly one.
///
/// The count to pass is the **collection's**, not the page's: `Showing 1 of 61 labels` is right
/// and `Showing 1 of 61 label` is not.
pub fn plural_s(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_limit_is_one_page_and_paginate_lifts_it() {
        assert_eq!(item_cap(&GlobalOpts::default()), Some(DEFAULT_LIMIT));
        let g = GlobalOpts { limit: Some(7), ..GlobalOpts::default() };
        assert_eq!(item_cap(&g), Some(7));
        let g = GlobalOpts { paginate: true, ..GlobalOpts::default() };
        assert_eq!(item_cap(&g), None, "--paginate with no --limit means everything");
        let g = GlobalOpts { paginate: true, limit: Some(5), ..GlobalOpts::default() };
        assert_eq!(item_cap(&g), Some(5));
    }

    #[test]
    fn the_limit_chain_prefers_the_local_flag() {
        let g = GlobalOpts { limit: Some(5), ..GlobalOpts::default() };
        assert_eq!(limit(Some(7), &g), 7);
        assert_eq!(limit(None, &g), 5);
        assert_eq!(limit(None, &GlobalOpts::default()), DEFAULT_LIMIT);
        // 0 items is not a request anyone means.
        assert_eq!(limit(Some(0), &GlobalOpts::default()), 1);
    }

    /// Bug this prevents: printing `Showing 2 of 2` for a list that was truncated, or inventing
    /// a total for a collection whose response carried no `x-total-count`.
    #[test]
    fn the_banner_only_claims_a_total_it_was_given() {
        assert_eq!(banner(2, Some(40), "webhooks"), "Showing 2 of 40 webhooks");
        assert_eq!(banner(2, None, "webhooks"), "Showing 2 webhooks");
        assert_eq!(banner(2, Some(2), "webhooks"), "Showing 2 webhooks");
    }

    /// Bug this prevents: spending a request on a total for a list that was never truncated,
    /// and — the reported bug — reporting no total at all for one that was.
    #[tokio::test]
    async fn the_total_is_free_for_a_short_list_and_probed_only_for_a_full_one() {
        let probed = std::cell::Cell::new(0);
        let probe = |_p: Paging| {
            probed.set(probed.get() + 1);
            std::future::ready(Ok((
                Vec::<u8>::new(),
                PageInfo { links: Default::default(), total_count: Some(61), returned: 1 },
            )))
        };

        // Short of the cap: the list is the whole collection, and nobody is asked.
        assert_eq!(total_if_truncated(7, 30, probe).await, Some(7));
        assert_eq!(probed.get(), 0, "a short list must not cost a request");

        // Full: there may be more, so the collection is asked how many.
        assert_eq!(total_if_truncated(30, 30, probe).await, Some(61));
        assert_eq!(probed.get(), 1);

        // `--paginate` with no `--limit` caps at usize::MAX and so never probes.
        assert_eq!(total_if_truncated(94, usize::MAX, probe).await, Some(94));
        assert_eq!(probed.get(), 1);
    }

    /// Bug this prevents: a banner probe that fails taking the whole listing down with it. The
    /// rows were fetched successfully; only the header is missing.
    #[tokio::test]
    async fn a_failed_probe_costs_the_total_and_nothing_else() {
        let refused = |_p: Paging| {
            std::future::ready(Err(gitea_core::error::Error::new(gitea_core::ErrorKind::Usage(
                "no".to_owned(),
            ))))
        };
        assert_eq!(total_if_truncated::<u8, _, _>(30, 30, refused).await, None);
        assert_eq!(banner(30, None, "labels"), "Showing 30 labels");
    }

    #[test]
    fn plurals_follow_the_collection_not_the_page() {
        assert_eq!(
            banner(1, Some(61), &format!("label{} in acme/w", plural_s(61))),
            "Showing 1 of 61 labels in acme/w"
        );
        assert_eq!(
            banner(1, Some(1), &format!("label{} in acme/w", plural_s(1))),
            "Showing 1 label in acme/w"
        );
    }

    /// Bug this prevents: a `Showing 30 of 30` banner on every complete list, which is noise,
    /// and a missing one when the list really was truncated.
    #[test]
    fn the_truncation_banner_only_appears_when_something_was_left_out() {
        assert_eq!(
            truncation_banner(30, Some(94), "pull requests").as_deref(),
            Some("Showing 30 of 94 pull requests")
        );
        assert_eq!(truncation_banner(30, Some(30), "pull requests"), None);
        assert_eq!(truncation_banner(30, None, "pull requests"), None);
    }
}
