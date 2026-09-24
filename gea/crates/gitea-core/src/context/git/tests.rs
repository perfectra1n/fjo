//! Unit tests for the git write surface. Everything here runs against [`FakeGit`]: no
//! repositories, no process spawns, no network.

use super::*;

fn lines(git: &FakeGit) -> Vec<String> {
    git.command_lines()
}

/// Unwrap the `GitFailed` an unsuccessful invocation must produce. Asserting on the variant's
/// own fields rather than on a rendered string is the point: the command, the exit status, and
/// git's stderr each have to survive individually.
fn git_failed(err: &crate::Error) -> (String, String, Option<i32>) {
    match err.kind() {
        ErrorKind::GitFailed { command, stderr, status } => {
            (command.clone(), stderr.clone(), *status)
        }
        other => panic!("expected ErrorKind::GitFailed, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------------- clone

#[test]
fn a_clone_shows_progress_and_reports_where_it_landed() {
    let git = FakeGit::repo();
    let dir = git
        .clone_repo(&CloneSpec::new("https://forge/them/proj.git"))
        .expect("the fake clone succeeds");
    assert_eq!(dir, PathBuf::from("proj"));
    let action = git.action_starting_with(&["clone"]).expect("a clone ran");
    assert_eq!(action.line(), "clone https://forge/them/proj.git");
    // Bug this prevents: capturing `git clone`'s progress meter, which turns a visible
    // thirty-second download into an apparently hung command.
    assert_eq!(action.io, GitIo::Inherit);
}

#[test]
fn a_clone_into_a_named_directory_passes_it_to_git() {
    let git = FakeGit::repo();
    let dir = git
        .clone_repo(&CloneSpec::new("https://forge/them/proj.git").into_dir("elsewhere"))
        .expect("clone");
    assert_eq!(dir, PathBuf::from("elsewhere"));
    assert_eq!(lines(&git), ["clone https://forge/them/proj.git elsewhere"]);
}

/// The whole point of shelling out: a failing git is reported in git's own words, not
/// replaced by "clone failed".
#[test]
fn a_failed_clone_carries_gits_own_stderr() {
    let git = FakeGit::repo().with_response(GitOutput::failure(128, "fatal: repository not found"));
    let err = git.clone_repo(&CloneSpec::new("https://forge/them/proj.git")).unwrap_err();
    let (command, stderr, status) = git_failed(&err);
    assert_eq!(command, "git clone https://forge/them/proj.git");
    assert_eq!(stderr, "fatal: repository not found");
    assert_eq!(status, Some(128));
}

/// Bug this prevents: an error quoting a clone URL with an embedded token, which the user
/// then pastes into a public issue.
#[test]
fn a_failed_clone_never_prints_an_embedded_credential() {
    let secret = "gho_aVeryRealLookingToken";
    let url = format!("https://me:{secret}@forge/them/proj.git");
    let git = FakeGit::repo()
        .with_response(GitOutput::failure(128, format!("fatal: could not read from '{url}'")));
    let err = git.clone_repo(&CloneSpec::new(&url)).unwrap_err();
    let (command, stderr, _) = git_failed(&err);
    // Both halves are covered: the argv we passed and the stderr git handed back.
    assert!(!command.contains(secret), "the token leaked into the command: {command}");
    assert!(!stderr.contains(secret), "the token leaked into the stderr: {stderr}");
    assert!(command.contains("me:<redacted>@forge"), "{command}");
    assert!(stderr.contains("me:<redacted>@forge"), "{stderr}");
}

// ------------------------------------------------------------------------------------- fetch

#[test]
fn a_fetch_renders_force_and_every_refspec() {
    let git = FakeGit::repo();
    git.fetch(
        &FetchSpec::new("upstream")
            .with_refspec("refs/pull/7/head")
            .with_refspec("main:main")
            .forced(true),
    )
    .expect("fetch");
    assert_eq!(lines(&git), ["fetch --force upstream refs/pull/7/head main:main"]);
}

/// A best-effort fetch (after adding an `upstream` remote to a `--filter=blob:none` clone)
/// must not draw a progress meter and then fail silently behind it.
#[test]
fn a_quiet_fetch_is_captured_rather_than_inherited() {
    let git = FakeGit::repo();
    git.fetch(&FetchSpec::new("upstream").quiet()).expect("fetch");
    assert_eq!(git.actions()[0].io, GitIo::Capture);
}

// -------------------------------------------------------------------------------------- push

#[test]
fn an_ordinary_push_can_set_the_upstream() {
    let git = FakeGit::repo();
    git.push_or_fail(&PushSpec::new("origin").with_refspec("HEAD").setting_upstream(true))
        .expect("push");
    assert_eq!(lines(&git), ["push -u origin HEAD"]);
}

/// A refused push is *not* an error from [`GitCtx::push`]: the caller has to be able to relay
/// the server's reply, which arrives on the stderr of the very invocation that failed.
#[test]
fn push_hands_back_a_refusal_instead_of_swallowing_it() {
    let git = FakeGit::repo()
        .with_response(GitOutput::failure(1, "! [rejected] main -> main (non-fast-forward)"));
    let out = git.push(&PushSpec::new("origin").with_refspec("HEAD")).expect("git ran");
    assert!(!out.ok);
    assert!(out.stderr.contains("non-fast-forward"));
    // ...and the failing variant turns the same thing into an error that keeps the words.
    let git = FakeGit::repo().with_response(GitOutput::failure(1, "! [rejected] nope"));
    let err = git.push_or_fail(&PushSpec::new("origin").with_refspec("HEAD")).unwrap_err();
    assert_eq!(git_failed(&err).1, "! [rejected] nope");
}

// -------------------------------------------------------------------------------------- AGit

/// The feature `gh` structurally cannot match: a pull request with no branch and no fork,
/// created entirely by a push to `refs/for/<base>/<topic>`.
#[test]
fn an_agit_push_is_a_push_to_refs_for_base_topic() {
    let git = FakeGit::repo().with_response(
        GitOutput::success().with_stderr("remote: https://forge/them/proj/pulls/7\n"),
    );
    let push = AgitPush::new("origin", AgitRef::new("main", "fix-parser").unwrap())
        .with_title("Fix the parser")
        .with_body("Fixes #12");

    let outcome = git.push_agit(&push).expect("the push is accepted");
    assert_eq!(outcome.pull_index, Some(7));
    assert!(outcome.stderr.contains("pulls/7"), "the server's reply is handed back verbatim");
    assert_eq!(
        lines(&git),
        [
            "push origin HEAD:refs/for/main/fix-parser -o title=Fix the parser -o description=Fixes #12"
        ]
    );
    // Captured, not inherited: the pull request URL is *in* that stderr.
    assert_eq!(git.actions()[0].io, GitIo::Capture);
}

/// Rule 3: an AGit update must fast-forward, so an amended history needs a force push.
#[test]
fn an_amended_agit_push_is_forced() {
    let git = FakeGit::repo();
    let push =
        AgitPush::new("origin", AgitRef::new("main", "t").unwrap()).with_title("x").forced(true);
    git.push_agit(&push).expect("push");
    assert_eq!(lines(&git), ["push --force origin HEAD:refs/for/main/t -o title=x"]);
}

/// Rule 2: the same topic updates the same pull request, so the refspec must be stable
/// across runs. A topic that silently changed would open a second pull request.
#[test]
fn the_same_topic_produces_the_same_refspec() {
    let a = AgitPush::new("origin", AgitRef::new("main", "fix").unwrap()).refspec();
    let b = AgitPush::new("origin", AgitRef::new("main", "fix").unwrap()).refspec();
    assert_eq!(a, b);
    assert_ne!(a, AgitPush::new("origin", AgitRef::new("main", "fix2").unwrap()).refspec());
}

/// A refusal keeps git's own words *and* adds the one piece of advice the message lacks.
///
/// Asserted on the RENDERED error rather than on `to_string()`: a headline is contractually one
/// plain line, so the advice and git's words now live in the facts and the "what to do" block
/// where the three-part shape puts them. `to_string()` would see only the first line.
#[test]
fn a_refused_agit_push_explains_itself_without_discarding_the_server() {
    let git = FakeGit::repo().with_response(GitOutput::failure(
        1,
        "remote: Gitea: user does not have permission\n\
         ! [remote rejected] HEAD -> refs/for/main/t (non-fast-forward)",
    ));
    let push = AgitPush::new("origin", AgitRef::new("main", "t").unwrap()).with_title("x");
    let err = git.push_agit(&push).unwrap_err();

    // The classification is what decides which remedy the user is given, so pin it directly
    // rather than inferring it from the prose.
    let ErrorKind::AgitRefused { refspec, remedy, .. } = err.kind() else {
        panic!("expected AgitRefused, got {:?}", err.kind());
    };
    assert_eq!(refspec, "HEAD:refs/for/main/t");
    assert_eq!(*remedy, AgitRemedy::ForcePush);

    let message = crate::error::render::render(&err, crate::error::render::Color::Never);
    assert!(message.contains("HEAD:refs/for/main/t"), "{message}");
    assert!(message.contains("force"), "{message}");
    assert!(message.contains("user does not have permission"), "{message}");
}

/// Rule 1, enforced before a round trip: Gitea refuses `refs/for/<base>` with no topic and
/// its own error does not say why.
#[test]
fn an_agit_push_without_a_topic_cannot_be_constructed() {
    assert!(AgitRef::new("main", "").is_err());
    assert!(AgitRef::new("main", "\t \n").is_err());
}

// ---------------------------------------------------------------------------------- checkout

#[test]
fn checkout_covers_switching_detaching_and_creating() {
    let git = FakeGit::repo();
    git.checkout(&Checkout::Rev("main".into())).expect("switch");
    git.checkout(&Checkout::Detach("FETCH_HEAD".into())).expect("detach");
    git.checkout(&Checkout::new_branch("pr/7").from_point("FETCH_HEAD")).expect("create");
    assert_eq!(
        lines(&git),
        ["checkout main", "checkout --detach FETCH_HEAD", "checkout -b pr/7 FETCH_HEAD",]
    );
}

/// The best-effort case: after a merge, switching away from the merged branch so it can be
/// deleted. Failing the whole command because a local checkout could not be switched would
/// report a failure for something that already succeeded on the server.
#[test]
fn try_checkout_reports_a_refusal_as_false() {
    let git = FakeGit::repo()
        .with_response(GitOutput::failure(1, "error: Your local changes would be overwritten"));
    assert!(!git.try_checkout(&Checkout::Rev("main".into())).expect("git ran"));
}

#[test]
fn branches_are_created_and_deleted() {
    let git = FakeGit::repo();
    git.create_branch("topic", Some("origin/main"), false).expect("create");
    git.create_branch("topic", None, true).expect("recreate");
    assert!(git.delete_branch("topic", true).expect("git ran"));
    assert_eq!(
        lines(&git),
        ["branch topic origin/main", "branch --force topic", "branch -D topic",]
    );
}

/// git refuses `-d` on an unmerged branch, and that refusal is information rather than a
/// failure — the caller decides whether losing commits is acceptable.
#[test]
fn deleting_an_unmerged_branch_is_a_false_not_an_error() {
    let git = FakeGit::repo()
        .with_response(GitOutput::failure(1, "error: the branch 'topic' is not fully merged"));
    assert!(!git.delete_branch("topic", false).expect("git ran"));
}

// ------------------------------------------------------------------------------------ config

/// The key the resolver's step 4 reads: `remote.<name>.gea-resolved`, written by
/// `gea repo set-default`.
#[test]
fn the_resolved_key_round_trips_through_config() {
    let git = FakeGit::repo();
    git.config_set_local("remote.origin.gea-resolved", "base").expect("write");
    assert_eq!(git.config_get("remote.origin.gea-resolved").unwrap().as_deref(), Some("base"));
    assert_eq!(
        git.config_snapshot().get("remote.origin.gea-resolved").map(String::as_str),
        Some("base")
    );
    git.config_unset_local("remote.origin.gea-resolved").expect("unset");
    assert_eq!(git.config_get("remote.origin.gea-resolved").unwrap(), None);
}

/// Bug this prevents: `gea repo set-default --unset` failing the second time it is run,
/// because git exits 5 for a key that was not there.
#[test]
fn unsetting_an_absent_key_is_not_a_failure() {
    let git = FakeGit::repo();
    git.config_unset_local("remote.origin.gea-resolved").expect("idempotent unset");
}

#[test]
fn config_regexp_finds_the_resolved_keys() {
    let git = FakeGit::repo()
        .with_config("remote.origin.gea-resolved", "base")
        .with_config("remote.origin.url", "https://x/y/z")
        .with_config("branch.main.remote", "origin");
    let found = git.config_get_regexp(r"^remote\..*\.gea-resolved$").unwrap();
    assert_eq!(found, vec![("remote.origin.gea-resolved".to_owned(), "base".to_owned())]);
}

// ----------------------------------------------------------------------------------- remotes

#[test]
fn adding_and_renaming_a_remote_changes_what_resolution_sees() {
    let git = FakeGit::repo().with_remote("origin", "https://forge/me/fork.git");
    assert!(git.remote_exists("origin").unwrap());
    assert!(!git.remote_exists("upstream").unwrap());

    git.remote_add("upstream", "https://forge/them/proj.git").expect("add");
    assert!(git.remote_exists("upstream").unwrap());

    // `gea repo fork --remote` renames the remote that pointed at the source.
    git.remote_rename("origin", "old").expect("rename");
    assert!(git.remote_exists("old").unwrap());
    assert!(!git.remote_exists("origin").unwrap());
}

// ----------------------------------------------------------------------------- derived reads

#[test]
fn rev_exists_asks_git_to_verify_quietly() {
    let git = FakeGit::repo().with_rev("upstream/main");
    assert!(git.rev_exists("upstream/main").unwrap());
    assert!(!git.rev_exists("upstream/nope").unwrap());
    // `--quiet` as well as `--verify`: without it a missing revision is a line of noise on
    // the user's terminal.
    assert_eq!(git.actions()[0].line(), "rev-parse --verify --quiet upstream/main");
}

/// `pr checkout` uses this to refuse clobbering a local branch that has commits the pull
/// request does not.
#[test]
fn is_ancestor_answers_can_this_fast_forward() {
    let git = FakeGit::repo().with_ancestor("pr/7", "FETCH_HEAD");
    assert!(git.is_ancestor("pr/7", "FETCH_HEAD").unwrap());
    assert!(!git.is_ancestor("FETCH_HEAD", "pr/7").unwrap());
}

/// Bug this prevents: `repo sync -f` running `git reset --hard` over uncommitted work.
/// `-f` is about the branch's history; nobody types it meaning "and delete the file I am
/// editing".
#[test]
fn a_dirty_work_tree_is_visible_before_a_hard_reset() {
    let clean = FakeGit::repo();
    assert!(clean.is_clean().unwrap());

    let dirty = FakeGit::repo().with_status(" M src/main.rs\n?? notes.txt");
    assert!(!dirty.is_clean().unwrap());
    assert!(dirty.porcelain_status().unwrap().contains("src/main.rs"));
}

#[test]
fn merge_ff_only_reports_a_divergence_as_false() {
    assert!(FakeGit::repo().merge_ff_only("origin/main").unwrap());
    assert!(!FakeGit::repo().with_diverged_branch().merge_ff_only("origin/main").unwrap());
}

/// Bug this prevents: `--fill` reading commits newest-first, so a two-commit branch's
/// generated body lists them in the reverse of the order they will be applied in.
#[test]
fn commit_subjects_are_oldest_first() {
    let git = FakeGit::repo()
        .with_log("origin/main..HEAD", &[("first thing", ""), ("second thing", "with a body")]);
    assert_eq!(git.commit_subjects("origin/main..HEAD").unwrap(), ["first thing", "second thing"]);
    // `%s` and not `%B`: four commit messages glued together is not a description.
    assert!(git.actions()[0].line().contains("--pretty=format:%s"));
    assert!(git.actions()[0].line().contains("--reverse"));
}

/// Bug this prevents: `--fill-first` taking the *last* commit's message, or gluing the
/// subject and body into one title.
#[test]
fn first_commit_message_splits_subject_from_body() {
    let git = FakeGit::repo().with_log(
        "origin/main..HEAD",
        &[("add the thing", "why the thing\nis needed"), ("later commit", "")],
    );
    let message = git.first_commit_message("origin/main..HEAD").unwrap().expect("a commit");
    assert_eq!(message.subject, "add the thing");
    assert_eq!(message.body, "why the thing\nis needed");
}

#[test]
fn an_empty_range_has_no_first_commit() {
    let git = FakeGit::repo().with_log("origin/main..HEAD", &[]);
    assert_eq!(git.first_commit_message("origin/main..HEAD").unwrap(), None);
    assert!(git.commit_subjects("origin/main..HEAD").unwrap().is_empty());
}

// ----------------------------------------------------------------------- worktree, submodules

#[test]
fn a_worktree_is_added_detached_so_the_branch_stays_put() {
    let git = FakeGit::repo();
    git.worktree_add(Path::new("../review-42"), "pr/42", true).expect("worktree");
    assert_eq!(lines(&git), ["worktree add --detach ../review-42 pr/42"]);
}

#[test]
fn submodules_are_initialised_as_well_as_updated() {
    let git = FakeGit::repo();
    git.update_submodules().expect("submodules");
    assert_eq!(lines(&git), ["submodule update --init --recursive"]);
    // Progress goes to the terminal: a submodule update can take a long time.
    assert_eq!(git.actions()[0].io, GitIo::Inherit);
}

// ------------------------------------------------------------------------------------- errors

/// A git that fails silently still has to produce a message that says what ran.
#[test]
fn a_silent_failure_still_names_the_command_and_the_exit_code() {
    let git = FakeGit::repo().with_response(GitOutput::failure(129, String::new()));
    let err = git.checkout(&Checkout::Rev("main".into())).unwrap_err();
    let (command, stderr, status) = git_failed(&err);
    assert_eq!(command, "git checkout main");
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(status, Some(129));
}

/// `GitCtx` must stay object-safe: `Runtime::git()` hands out a `&dyn GitCtx`, and a
/// non-object-safe method added here would break every call site in the binary.
#[test]
fn the_trait_is_object_safe() {
    let git = FakeGit::repo();
    let dynamic: &dyn GitCtx = &git;
    dynamic.checkout(&Checkout::Rev("main".into())).expect("checkout through a trait object");
    assert_eq!(lines(&git), ["checkout main"]);
}
