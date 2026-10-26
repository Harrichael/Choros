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
    Queue::create(&w.trunk, "epicq", QueueConfig::new(REPO, "main", Scope::Branch("epic-x".into()))).unwrap();
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

    let retire = |q: &Queue| {
        let work = crate::engine::Work::begin(q, &w.agent, None, true).unwrap().unwrap();
        crate::queue::retire_if_orphaned(q, &work.st).unwrap()
    };
    space::archive_branch(&epic).unwrap();
    assert!(!retire(&epicq), "a set is still open");

    w.forge.merge(pr, MergeMethod::Merge);
    crate::admin::sync(&w.env(), &epicq, None).unwrap();
    assert_eq!(epicq.read_state().unwrap().sets[&1].status, SetStatus::Merged);
    assert!(retire(&epicq));
    assert_eq!(crate::queue::list(&w.trunk).unwrap(), [QUEUE]);
    assert!(w.trunk.join(".choros-config/archive/landing/epicq/_queue/queue.toml").exists());
    assert!(!retire(&w.queue()), "trunk queues stay");
}

/// `land archive` retires the workspace it ran in, not a same-named one in
/// another space.
#[test]
fn land_archive_retires_the_workspace_it_ran_in() {
    let w = World::new();
    let at_trunk = w.workspace("login");
    let epic = space::create_branch(&w.trunk, "epic-x").unwrap();
    let in_epic = choros_core::choros::create(&epic, "login", &[REPO.to_string()], &()).unwrap().path;
    crate::land::bind(&w.env(), &in_epic, REPO, Some(QUEUE)).unwrap();
    w.commit(&in_epic, "a.txt", "a\n");

    let req = LandRequest {
        cwd: in_epic.join(REPO),
        mode: LandMode::Archive,
        fork: None,
        on_conflict: None,
        title: None,
        body: None,
        sessions: vec![],
        env_session: None,
    };
    let out = crate::land::land(&w.env(), &req, None).unwrap();
    assert_eq!(out.archived_to, Some(w.trunk.join(".choros-config/archive/epic-x/login")));
    assert!(!in_epic.exists());
    assert!(at_trunk.exists());
}
