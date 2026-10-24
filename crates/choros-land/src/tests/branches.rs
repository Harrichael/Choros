use super::*;
use crate::state::SetStatus;
use choros_core::space;

/// A queue scoped to a branch-space takes landings only from workspaces in
/// it, is what new workspaces there bind to by default, is all `status` shows
/// from inside it, and goes to the archive once its branch-space has been
/// archived and its last set has merged.
#[test]
fn a_branch_space_queue_lives_and_retires_with_its_space() {
    let w = World::new();
    let epic = space::create_branch(&w.trunk, "epic-x").unwrap();
    Queue::create(&w.trunk, "epicq", REPO, Scope::Branch("epic-x".into())).unwrap();
    crate::branch::set_binding(&epic, REPO, Some("epicq")).unwrap();
    assert_eq!(crate::branch::default_binding(&epic, REPO).unwrap(), Some(Some("epicq".into())));

    let outside = w.workspace("outside");
    assert!(crate::land::bind(&w.env(), &outside, REPO, Some("epicq")).is_err());

    let info = choros_core::choros::create(&epic, "login", &[REPO.to_string()], &()).unwrap();
    assert_eq!(info.meta.branch(), "epic-x/login");
    let login = info.path;
    crate::land::bind(&w.env(), &login, REPO, Some("epicq")).unwrap();

    let seen = |cwd: &Path| -> Vec<String> {
        crate::status::visible_queues(cwd, None, false).unwrap().into_iter().map(|(_, q)| q).collect()
    };
    assert_eq!(seen(&epic), ["epicq"]);
    assert_eq!(seen(&w.trunk), [QUEUE, "epicq"]);

    w.commit(&login, "a.txt", "a\n");
    w.land(&login).unwrap();
    let epicq = Queue::open(&w.trunk, "epicq").unwrap();
    let pr = epicq.read_state().unwrap().sets[&1].pr.as_ref().unwrap().number;

    space::archive_branch(&epic).unwrap();
    assert!(!crate::queue::retire_if_orphaned(&w.trunk, "epicq").unwrap(), "a set is still open");

    w.forge.merge(pr, MergeMethod::Merge);
    crate::admin::sync(&w.env(), &epicq, None).unwrap();
    assert_eq!(epicq.read_state().unwrap().sets[&1].status, SetStatus::Merged);
    assert!(crate::queue::retire_if_orphaned(&w.trunk, "epicq").unwrap());
    assert_eq!(crate::queue::list(&w.trunk).unwrap(), [QUEUE]);
    assert!(w.trunk.join(".choros-config/archive/landing/epicq/_queue/queue.toml").exists());
    assert!(!crate::queue::retire_if_orphaned(&w.trunk, QUEUE).unwrap(), "trunk queues stay");
}
