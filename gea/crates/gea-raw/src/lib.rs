//! Layer 2: all 506 operations as clap commands, built at runtime from metadata.
//!
//! The engine is four small modules and no generated code:
//!
//! * [`mod@build`] — [`peek`] decides in nanoseconds whether this invocation is layer 2 at all,
//!   and [`build`](build::build) then constructs *only* the named subtree of `clap::Command`s.
//! * [`mod@bind`] — [`clap::ArgMatches`] to a [`PlannedRequest`]: method, rendered path, ordered
//!   query pairs, body JSON. No I/O; the binary sends it.
//! * [`bodyfile`] — `--body-file` supplies the base body, flags override it by JSON pointer.
//! * [`search`] — `gea raw search <words>`, so 506 commands are discoverable.
//!
//! Every entry point takes the metadata tables as parameters (`ops: &'static [OpMeta]`,
//! `groups: &'static [GroupMeta]`) rather than reading the generated statics directly. That is
//! not indirection for its own sake: it lets this crate be unit-tested against hand-written
//! fixtures, and it means the emitter and the engine can be developed and reviewed
//! independently.
//!
//! ```ignore
//! // in gea's main:
//! if let Some(x) = gea_raw::peek(&argv) {
//!     let cmd = gea_raw::build(OPS, GROUPS, x.group, x.leaf);
//!     let m = cmd.try_get_matches_from(&argv)?;
//!     // descend to the leaf's matches, then:
//!     let plan = gea_raw::bind(op, leaf_matches, repo_ctx.as_ref(), &mut std::io::stdin())?;
//! }
//! ```
// This crate is `publish = false`, so its rustdoc exists for contributors, who read it with
// `--document-private-items`. Module docs here deliberately link to the private helpers they
// describe — that is the useful thing to link to when explaining how a module works — and those
// links resolve under that flag. Suppressing the lint keeps the links navigable rather than
// demoting sixteen of them to inert code spans. The published crates (gitea-core, -model,
// -client) do NOT carry this allow: docs.rs renders no private items, so there a link to one is
// genuinely broken for the only audience that sees it.
#![allow(rustdoc::private_intra_doc_links)]
#![forbid(unsafe_code)]

pub mod bind;
pub mod bodyfile;
pub mod build;
pub mod search;

pub use bind::{PlannedRequest, bind};
pub use build::{Peeked, build, peek, raw_command};

/// Hand-written metadata standing in for the generated `OPS`/`GROUPS` tables.
///
/// These exist so the engine is testable without the emitter, and they are chosen to cover the
/// shapes that have historically broken this kind of code rather than to be representative:
/// a `PathLike` parameter, the dotted `/{index}.{diffType}` path, a `limit` query parameter
/// that collides with `--limit`, a body with `deep` fields, a multipart upload, a deprecated
/// operation, and a path parameter with no `ctx_fill`.
///
/// The invariants the real tables must also hold: `OPS` sorted by `(group, command)`, `GROUPS`
/// sorted by `name`, and each group's `first`/`len` naming a contiguous run of `OPS`.
#[cfg(test)]
pub(crate) mod fixtures {
    use gitea_client::meta_types::*;

    const fn param(wire: &'static str, flag: &'static str, location: In, ty: ValueTy) -> ParamMeta {
        ParamMeta {
            wire,
            flag,
            location,
            ty,
            required: false,
            repeatable: false,
            encoding: PathEncoding::Segment,
            ctx_fill: None,
            enum_values: &[],
            help: "",
        }
    }

    const fn owner() -> ParamMeta {
        ParamMeta {
            required: true,
            ctx_fill: Some(CtxFill::Owner),
            help: "owner of the repository",
            ..param("owner", "owner", In::Path, ValueTy::Str)
        }
    }

    const fn repo() -> ParamMeta {
        ParamMeta {
            required: true,
            ctx_fill: Some(CtxFill::Repo),
            help: "name of the repository",
            ..param("repo", "repo", In::Path, ValueTy::Str)
        }
    }

    const REPO_ONLY: &[ParamMeta] = &[owner(), repo()];

    const CONTENTS: &[ParamMeta] = &[
        owner(),
        repo(),
        ParamMeta {
            required: true,
            encoding: PathEncoding::PathLike,
            help: "path of the file",
            ..param("filepath", "filepath", In::Path, ValueTy::Str)
        },
    ];

    const DIFF: &[ParamMeta] = &[
        owner(),
        repo(),
        ParamMeta { required: true, ..param("index", "index", In::Path, ValueTy::Int) },
        ParamMeta {
            required: true,
            enum_values: &["diff", "patch"],
            ..param("diffType", "diff-type", In::Path, ValueTy::Str)
        },
    ];

    const COMMENT: &[ParamMeta] = &[
        owner(),
        repo(),
        ParamMeta {
            required: true,
            help: "index of the issue",
            ..param("index", "index", In::Path, ValueTy::Int)
        },
    ];

    const ISSUE_LIST: &[ParamMeta] = &[
        owner(),
        repo(),
        ParamMeta {
            enum_values: &["open", "closed", "all"],
            help: "state of the issues",
            ..param("state", "state", In::Query, ValueTy::Str)
        },
        ParamMeta { repeatable: true, ..param("labels", "labels", In::Query, ValueTy::List) },
        ParamMeta { ..param("mine", "mine", In::Query, ValueTy::Bool) },
        ParamMeta { ..param("page", "page", In::Query, ValueTy::Int) },
        // Collides with the reserved `--limit`; must come out as `--per-page`.
        ParamMeta { ..param("limit", "limit", In::Query, ValueTy::Int) },
    ];

    const ATTACHMENT: &[ParamMeta] = &[
        owner(),
        repo(),
        ParamMeta { required: true, ..param("id", "id", In::Path, ValueTy::Int) },
        // `name` really is a query parameter on this operation, not a form field — a good
        // reminder that "multipart upload" does not mean "everything travels in the body".
        ParamMeta { ..param("name", "name", In::Query, ValueTy::Str) },
        ParamMeta {
            required: true,
            ..param("attachment", "attachment", In::FormData, ValueTy::File)
        },
        // Synthetic, to cover the scalar form-field path alongside the file one.
        ParamMeta { ..param("checksum", "checksum", In::FormData, ValueTy::Str) },
    ];

    const fn field(
        pointer: &'static str,
        flag: &'static str,
        ty: ValueTy,
        required: bool,
    ) -> BodyField {
        BodyField { pointer, flag, ty, required, enum_values: &[], help: "" }
    }

    const CREATE_PR_BODY: BodyMeta = BodyMeta {
        type_name: "CreatePullRequestOption",
        required: true,
        content_type: "application/json",
        fields: &[
            field("/title", "title", ValueTy::Str, true),
            field("/body", "body", ValueTy::Str, false),
            field("/head", "head", ValueTy::Str, false),
            field("/base", "base", ValueTy::Str, false),
            field("/draft", "draft", ValueTy::Bool, false),
            field("/assignees", "assignees", ValueTy::List, false),
            field("/milestone", "milestone", ValueTy::Int, false),
        ],
        deep: &["milestone_object", "labels_detail"],
    };

    const COMMENT_BODY: BodyMeta = BodyMeta {
        type_name: "CreateIssueCommentOption",
        required: true,
        content_type: "application/json",
        fields: &[field("/body", "body", ValueTy::Str, true)],
        deep: &[],
    };

    const MARKUP_BODY: BodyMeta = BodyMeta {
        type_name: "MarkupOption",
        required: false,
        content_type: "application/json",
        fields: &[
            field("/text", "text", ValueTy::Str, false),
            field("/context", "context", ValueTy::Json, false),
        ],
        deep: &[],
    };

    const fn op_meta(
        op_id: &'static str,
        group: &'static str,
        command: &'static str,
        method: &'static str,
        path: &'static str,
        params: &'static [ParamMeta],
    ) -> OpMeta {
        OpMeta {
            op_id,
            group,
            command,
            method,
            path,
            summary: "",
            description: "",
            params,
            body: None,
            pagination: Pagination::None,
            produces: Produces::Json,
            scope: None,
            deprecated: None,
        }
    }

    /// Sorted by `(group, command)`, as `lookup::op`'s binary search requires.
    pub(crate) static OPS: &[OpMeta] = &[
        // ---- issue
        OpMeta {
            summary: "Add a comment to an issue",
            body: Some(&COMMENT_BODY),
            scope: Some("write:issue"),
            ..op_meta(
                "issueCreateComment",
                "issue",
                "create-comment",
                "POST",
                "/repos/{owner}/{repo}/issues/{index}/comments",
                COMMENT,
            )
        },
        OpMeta {
            summary: "List a repository's issues",
            pagination: Pagination::Paged,
            scope: Some("read:issue"),
            ..op_meta(
                "issueListIssues",
                "issue",
                "list",
                "GET",
                "/repos/{owner}/{repo}/issues",
                ISSUE_LIST,
            )
        },
        // ---- misc
        OpMeta {
            summary: "Render raw markdown as HTML",
            body: Some(&MARKUP_BODY),
            produces: Produces::Html,
            deprecated: Some("use `misc markup` instead"),
            ..op_meta("renderMarkdownRaw", "misc", "markdown-raw", "POST", "/markdown/raw", &[])
        },
        // ---- repo
        OpMeta {
            summary: "Create a pull request",
            body: Some(&CREATE_PR_BODY),
            scope: Some("write:repository"),
            ..op_meta(
                "repoCreatePullRequest",
                "repo",
                "create-pull-request",
                "POST",
                "/repos/{owner}/{repo}/pulls",
                REPO_ONLY,
            )
        },
        OpMeta {
            summary: "Create a release attachment",
            scope: Some("write:repository"),
            ..op_meta(
                "repoCreateReleaseAttachment",
                "repo",
                "create-release-attachment",
                "POST",
                "/repos/{owner}/{repo}/releases/{id}/assets",
                ATTACHMENT,
            )
        },
        OpMeta {
            summary: "Get a pull request diff or patch",
            produces: Produces::Text,
            ..op_meta(
                "repoDownloadPullDiffOrPatch",
                "repo",
                "download-pull-diff-or-patch",
                "GET",
                "/repos/{owner}/{repo}/pulls/{index}.{diffType}",
                DIFF,
            )
        },
        OpMeta {
            summary: "Get a repository",
            scope: Some("read:repository"),
            ..op_meta("repoGet", "repo", "get", "GET", "/repos/{owner}/{repo}", REPO_ONLY)
        },
        OpMeta {
            summary: "Get the contents of a file or directory",
            ..op_meta(
                "repoGetContents",
                "repo",
                "get-contents",
                "GET",
                "/repos/{owner}/{repo}/contents/{filepath}",
                CONTENTS,
            )
        },
        // ---- user
        OpMeta {
            summary: "Get the authenticated user",
            ..op_meta("userGetCurrent", "user", "get-current", "GET", "/user", &[])
        },
    ];

    /// Sorted by `name`; `first`/`len` name contiguous runs of [`OPS`].
    pub(crate) static GROUPS: &[GroupMeta] = &[
        GroupMeta { name: "issue", about: "Issues, comments, labels", first: 0, len: 2 },
        GroupMeta { name: "misc", about: "Markup, version, signing key", first: 2, len: 1 },
        GroupMeta { name: "repo", about: "Repositories and their contents", first: 3, len: 5 },
        GroupMeta { name: "user", about: "Users and their settings", first: 8, len: 1 },
    ];

    /// One valid invocation per operation, for tests that must exercise all of them.
    pub(crate) const EVERY_OP_INVOCATION: &[(&str, &str, &[&str])] = &[
        ("issue", "create-comment", &["o", "r", "1"]),
        ("issue", "list", &["o", "r"]),
        ("misc", "markdown-raw", &[]),
        ("repo", "create-pull-request", &["o", "r"]),
        ("repo", "create-release-attachment", &["o", "r", "7"]),
        ("repo", "download-pull-diff-or-patch", &["o", "r", "1", "diff"]),
        ("repo", "get", &["o", "r"]),
        ("repo", "get-contents", &["o", "r", "README.md"]),
        ("user", "get-current", &[]),
    ];

    pub(crate) fn op(group: &str, command: &str) -> &'static OpMeta {
        lookup::op(OPS, group, command).unwrap_or_else(|| panic!("no fixture op {group} {command}"))
    }

    /// Parse a leaf invocation the way the binary will: build the subtree, parse the full
    /// `argv`, then descend to the leaf's matches.
    pub(crate) fn matches(group: &str, command: &str, words: &[&str]) -> clap::ArgMatches {
        let cmd = crate::build::build(OPS, GROUPS, Some(group), Some(command));
        let mut argv: Vec<String> = vec!["gea".into(), "raw".into(), group.into(), command.into()];
        argv.extend(words.iter().map(|w| (*w).to_owned()));
        let m = cmd
            .try_get_matches_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?} did not parse: {e}"));
        m.subcommand_matches("raw")
            .and_then(|m| m.subcommand_matches(group))
            .and_then(|m| m.subcommand_matches(command))
            .expect("leaf matches")
            .clone()
    }

    pub(crate) fn leaf_help(group: &str, command: &str) -> String {
        let mut cmd = crate::build::build(OPS, GROUPS, Some(group), Some(command));
        cmd.find_subcommand_mut("raw")
            .unwrap()
            .find_subcommand_mut(group)
            .unwrap()
            .find_subcommand_mut(command)
            .unwrap()
            .render_long_help()
            .to_string()
    }

    #[test]
    fn the_fixture_table_holds_the_invariants_the_real_one_must() {
        assert!(OPS.windows(2).all(|w| (w[0].group, w[0].command) < (w[1].group, w[1].command)));
        assert!(GROUPS.windows(2).all(|w| w[0].name < w[1].name));
        assert_eq!(GROUPS.iter().map(|g| g.len).sum::<usize>(), OPS.len());
        for g in GROUPS {
            assert!(lookup::ops_in(OPS, g).iter().all(|o| o.group == g.name), "{}", g.name);
        }
        assert_eq!(EVERY_OP_INVOCATION.len(), OPS.len());
    }
}
