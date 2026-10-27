use super::*;
use crate::state::SetStatus;
use choros_core::space;

/// A queue scoped to a branch-space takes every workspace in it without
/// being bound, and no other; it is the only queue a branch-space can have,
/// all `status` shows from inside it, and it goes to the archive once its
/// branch-space has been archived and its last set has merged.
#[test]
fn a_branch_space_queue_lives_and_retires_with_its_space() {
    let w = World::new();
    let epic = space::create_branch(&w.trunk, "epic-x").unwrap();
    let scope = Scope::Branch("epic-x".into());
    Queue::create(&w.trunk, "epicq", QueueConfig::new(scope.clone())).unwrap();
    assert!(Queue::create(&w.trunk, "second", QueueConfig::new(scope)).is_err());

    let outside = w.workspace("outside");
    assert!(crate::land::bind(&w.env(), &outside, REPO, Some("epicq")).is_err());

    let info = choros_core::choros::create(&epic, "login", &[REPO.to_string()], &()).unwrap();
    assert_eq!(info.meta.branch(), "epic-x/login");
    let login = info.path;
    assert_eq!(crate::workspace::queue_for(&w.trunk, &login, REPO).unwrap().as_deref(), Some("epicq"));

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
        labels: Vec::new(),
        sessions: vec![],
        env_session: None,
    };
    let out = w.land_req(&req, None).unwrap();
    assert_eq!(out.archived_to, Some(w.trunk.join(".choros-config/archive/epic-x/login")));
    assert!(!in_epic.exists());
    assert!(at_trunk.exists());
}
