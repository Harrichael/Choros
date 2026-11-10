//! A conflict that can be settled without judgment is, before anyone is
//! asked: by a rule in queue.toml, by the file's syntax tree, or the way
//! this queue settled the same three versions before.

use super::*;
use crate::state::MergeRule;

/// Two sets at the front that come to rewrite `file` each its own way, with
/// no one there to resolve the clash: the second is parked out of the merge
/// head.
fn clash(w: &World, file: &str, one: &str, two: &str) -> u32 {
    let (a, b) = (w.workspace("one"), w.workspace("two"));
    w.commit(&a, file, one);
    w.land_with(&a, Some(vec![]), None).unwrap();
    let n = w.clash_beside(&b, writes(file, two));
    assert!(
        w.state().sets[&n].park.is_some(),
        "no one there to resolve it"
    );
    n
}

fn rule(w: &World, rule: MergeRule) {
    let mut config = w.queue().config;
    config.merge.rules.push(rule);
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
}

fn merge_head(w: &World) -> String {
    w.state().repos[REPO]
        .merge_head
        .as_ref()
        .unwrap()
        .sha
        .clone()
}

/// What settled `file` in the merge head.
fn settled_by(w: &World, file: &str) -> Vec<&'static str> {
    let settled = w.queue().settled_in(REPO, &[merge_head(w)]).unwrap();
    settled
        .into_iter()
        .filter(|m| m.path == file)
        .map(|m| m.by.name())
        .collect()
}

/// A file with a `union` rule keeps both sides' lines. The daemon settles
/// it with no agent, and trusts it because the repo's check passes.
#[test]
fn a_union_rule_settles_a_clash_without_the_agent() {
    let w = World::new();
    let n = clash(&w, "README", "one\n", "two\n");
    rule(
        &w,
        MergeRule {
            files: "README".into(),
            union: true,
            regenerate: None,
        },
    );

    w.daemon_pass();

    assert!(w.state().sets[&n].park.is_none());
    assert_eq!(
        w.file_at(&merge_head(&w), "README").as_deref(),
        Some("one\ntwo\n")
    );
    assert!(
        w.agent.models.lock().unwrap().is_empty(),
        "the agent was not needed"
    );
    assert_eq!(settled_by(&w, "README"), ["union"]);
}

/// A fix no one judged stands only if the check passes on it. When it
/// fails, the conflict goes on to the agent as it was.
#[test]
fn a_mechanical_fix_the_check_fails_goes_to_the_agent() {
    let w = World::new();
    let n = clash(&w, "README", "one\n", "two\n");
    rule(
        &w,
        MergeRule {
            files: "README".into(),
            union: true,
            regenerate: None,
        },
    );
    w.repos_toml("[app]\ncheck = \"! grep -q two README\"\n");
    *w.agent.fix.lock().unwrap() = Some("one alone\n".into());

    w.daemon_pass();

    assert!(w.state().sets[&n].park.is_none());
    assert_eq!(
        w.file_at(&merge_head(&w), "README").as_deref(),
        Some("one alone\n")
    );
    assert_eq!(settled_by(&w, "README"), ["agent"]);
}

/// What a person settles is kept by the file's three versions. When they
/// meet again the queue settles them the same way with no one there, and
/// without git's own memory of the fix.
#[test]
fn a_conflict_settled_once_is_settled_the_same_way_again() {
    let w = World::new();
    let n = clash(&w, "README", "one\n", "two\n");
    let mut by_hand = Scripted::default();
    by_hand
        .content
        .insert("README".into(), "one, then two\n".into());
    w.resolve(&mut by_hand);
    assert_eq!(settled_by(&w, "README"), ["hand"]);

    git(
        &w.queue().sync_dir(REPO),
        &["config", "rerere.enabled", "false"],
    );
    w.push_to_main("other.txt", "other\n");
    w.pass();

    assert!(w.state().sets[&n].park.is_none());
    assert_eq!(
        w.file_at(&merge_head(&w), "README").as_deref(),
        Some("one, then two\n")
    );
    assert_eq!(
        w.file_at(&merge_head(&w), "other.txt").as_deref(),
        Some("other\n")
    );
}

/// An answer that took judgment was right for the tree it was given in.
/// Met again, it stands only if the check passes on the tree it is met in.
#[test]
fn a_judged_answer_met_again_must_pass_the_check_again() {
    let w = World::new();
    let n = clash(&w, "README", "one\n", "two\n");
    let mut by_hand = Scripted::default();
    by_hand
        .content
        .insert("README".into(), "one, then two\n".into());
    w.resolve(&mut by_hand);

    git(
        &w.queue().sync_dir(REPO),
        &["config", "rerere.enabled", "false"],
    );
    w.repos_toml("[app]\ncheck = \"! test -e other.txt\"\n");
    w.push_to_main("other.txt", "other\n");
    w.pass();

    assert!(
        w.state().sets[&n].park.is_some(),
        "no one there to judge it again"
    );
}

/// A rule that could never apply is refused when the queue is opened, not
/// passed over.
#[test]
fn a_rule_that_cannot_apply_is_refused() {
    let w = World::new();
    rule(
        &w,
        MergeRule {
            files: "README".into(),
            union: false,
            regenerate: None,
        },
    );
    let err = Queue::open(&w.trunk, QUEUE).err().unwrap().to_string();
    assert!(format!("{err:#}").contains("queue.toml"), "{err}");
}

/// Mergiraf merges by the file's syntax tree: an entry changed by one fork
/// and another added right below it by the other clash as lines and not as
/// a tree. Such a clash is settled as the second fork lands, with no one
/// asked and nothing parked.
#[test]
fn a_clash_of_lines_but_not_of_syntax_is_settled_by_mergiraf() {
    if crate::mend::mergiraf().is_none() {
        eprintln!("skipped: mergiraf is not installed");
        return;
    }
    let w = World::new();
    w.push_to_main(
        "stacks.json",
        "{\n  \"auth\": [\"a\"],\n  \"mail\": [\"m\"]\n}\n",
    );
    let (a, b) = (w.workspace("one"), w.workspace("two"));
    w.commit(
        &a,
        "stacks.json",
        "{\n  \"auth\": [\"a\"],\n  \"mail\": [\"m\", \"goose\"]\n}\n",
    );
    w.land_with(&a, Some(vec![]), None).unwrap();
    w.clash_beside(
        &b,
        writes(
            "stacks.json",
            "{\n  \"auth\": [\"a\"],\n  \"mail\": [\"m\"],\n  \"content\": [\"c\"]\n}\n",
        ),
    );

    assert!(w.state().sets.values().all(|s| s.park.is_none()));
    assert_eq!(
        w.file_at(&merge_head(&w), "stacks.json").as_deref(),
        Some(
            "{\n  \"auth\": [\"a\"],\n  \"mail\": [\"m\", \"goose\"],\n  \"content\": [\"c\"]\n}\n"
        )
    );
    assert_eq!(settled_by(&w, "stacks.json"), ["mergiraf"]);
}

/// Files made from the rest of the tree are made again, each by its rule's
/// command, once everything else is settled: here each fork changed one
/// input and both files made from the two.
#[test]
fn regenerate_rules_make_files_again_from_both_sides() {
    let w = World::new();
    w.push_to_main("a.txt", "a\n");
    w.push_to_main("b.txt", "b\n");
    w.push_to_main("sum.txt", "a\nb\n");
    w.push_to_main("rev.txt", "b\na\n");
    let made_by = |files: &str, command: &str| MergeRule {
        files: files.into(),
        union: false,
        regenerate: Some(command.into()),
    };
    rule(&w, made_by("sum.txt", "cat a.txt b.txt > sum.txt"));
    rule(
        &w,
        made_by("rev.txt", "cat b.txt a.txt > rev.txt && touch stray.txt"),
    );
    let (one, two) = (w.workspace("one"), w.workspace("two"));
    w.commit(&one, "a.txt", "a1\n");
    w.commit(&one, "sum.txt", "a1\nb\n");
    w.commit(&one, "rev.txt", "b\na1\n");
    w.land_with(&one, Some(vec![]), None).unwrap();
    w.clash_beside(&two, |dir| {
        for (file, content) in [("b.txt", "b2\n"), ("sum.txt", "a\nb2\n"), ("rev.txt", "b2\na\n")] {
            std::fs::write(dir.join(file), content).unwrap();
        }
    });

    assert!(w.state().sets.values().all(|s| s.park.is_none()));
    assert_eq!(
        w.file_at(&merge_head(&w), "sum.txt").as_deref(),
        Some("a1\nb2\n")
    );
    assert_eq!(
        w.file_at(&merge_head(&w), "rev.txt").as_deref(),
        Some("b2\na1\n")
    );
    assert_eq!(w.file_at(&merge_head(&w), "stray.txt"), None);
    assert_eq!(settled_by(&w, "sum.txt"), ["regenerate"]);
}

/// A command that fails settles nothing: the conflict is as git left it,
/// for whoever is asked next, and with no one to ask it parks.
#[test]
fn a_regenerate_command_that_fails_leaves_the_conflict_to_be_judged() {
    let w = World::new();
    let made_by = |files: &str, command: &str| MergeRule {
        files: files.into(),
        union: false,
        regenerate: Some(command.into()),
    };
    rule(&w, made_by("README", "false"));
    let n = clash(&w, "README", "one\n", "two\n");

    let mut by_hand = Scripted::default();
    by_hand
        .content
        .insert("README".into(), "one, then two\n".into());
    w.resolve(&mut by_hand);

    assert!(w.state().sets[&n].park.is_none());
    assert_eq!(by_hand.seen[0].files, ["README"]);
    assert_eq!(
        w.file_at(&merge_head(&w), "README").as_deref(),
        Some("one, then two\n")
    );
}
