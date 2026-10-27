use super::*;
use crate::state::SetStatus;

/// A workspace holding two repos lands both from its root: each repo
/// becomes its own landing-set with its own number, built and pushed in that
/// repo, and each repo gets a merge head of its own. A new set depends only
/// on what is queued in its own repo.
#[test]
fn landing_two_repos_makes_a_set_in_each() {
    let w = World::new();
    let lib_origin = w.add_repo("lib");
    choros_core::choros::create(&w.trunk, "ws", &[REPO.to_string(), "lib".to_string()], &()).unwrap();
    let ws = w.trunk.join("ws");
    for repo in [REPO, "lib"] {
        crate::land::bind(&w.env(), &ws, repo, Some(QUEUE)).unwrap();
    }
    w.commit_in(&ws, REPO, "a.txt", "a\n");
    w.commit_in(&ws, "lib", "l.txt", "l\n");

    let req = LandRequest {
        cwd: ws.clone(),
        mode: LandMode::Continue,
        fork: None,
        on_conflict: None,
        title: None,
        body: None,
        labels: Vec::new(),
        sessions: vec![],
        env_session: None,
    };
    let out = w.land_req(&req, None).unwrap();
    let mut landed: Vec<(String, u32)> = out.landed.iter().map(|l| (l.repo.clone(), l.n.unwrap())).collect();
    landed.sort();
    assert_eq!(landed, [(REPO.to_string(), 1), ("lib".to_string(), 2)]);

    let st = w.state();
    assert_eq!(st.sets[&1].repo(), REPO);
    assert_eq!(st.sets[&2].repo(), "lib");
    assert!(st.sets[&2].deps.is_empty(), "nothing queued in lib before it");
    assert_eq!(w.file_at_in("lib", &st.sets[&2].tip, "l.txt").as_deref(), Some("l\n"));
    assert!(w.file_at_in("lib", &st.sets[&2].tip, "a.txt").is_none());
    let lib_mh = &st.repos["lib"].merge_head.as_ref().unwrap().sha;
    assert_eq!(git(&lib_origin, &["rev-parse", "appq/merge-head"]), *lib_mh);
    assert!(st.sets[&1].pr.is_some() && st.sets[&2].pr.is_some(), "both at the front of their repo");

    w.commit_in(&ws, "lib", "m.txt", "m\n");
    let req = LandRequest { cwd: ws.join("lib"), ..req };
    w.land_req(&req, None).unwrap();
    assert_eq!(w.state().sets[&3].deps, [Dep::Set(2)], "only lib's sets");
}

/// A set can wait on one in another repo. It can't build on it, so its base
/// stays its own repo's main, but its PR opens only once the other merges.
#[test]
fn a_dependency_across_repos_orders_the_prs() {
    let w = World::new();
    w.add_repo("lib");
    choros_core::choros::create(&w.trunk, "ws", &[REPO.to_string(), "lib".to_string()], &()).unwrap();
    let ws = w.trunk.join("ws");
    for repo in [REPO, "lib"] {
        crate::land::bind(&w.env(), &ws, repo, Some(QUEUE)).unwrap();
    }
    w.commit_in(&ws, "lib", "api.txt", "api\n");
    let lib_req = LandRequest {
        cwd: ws.join("lib"),
        mode: LandMode::Continue,
        fork: None,
        on_conflict: None,
        title: None,
        body: None,
        labels: Vec::new(),
        sessions: vec![],
        env_session: None,
    };
    crate::land::land(&w.env(), &lib_req, None).unwrap();
    w.commit(&ws, "uses.txt", "uses the api\n");
    w.land_with(&ws, Some(vec![Dep::Set(1)]), None, None).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
    assert!(st.sets[&2].base.deps.is_empty(), "nothing of lib's in app's base");
    assert!(st.sets[&2].pr.is_none(), "waits for lib's PR");

    w.forge.merge(st.sets[&1].pr.as_ref().unwrap().number, MergeMethod::Merge);
    let report = w.pass();
    assert_eq!(report.opened, [2]);
    assert_eq!(w.state().sets[&1].status, SetStatus::Merged);
}

/// A queue made when a queue held one repo keeps working: its repo, its
/// branch and what it had built are read into the per-repo state.
#[test]
fn a_one_repo_queue_from_before_is_read_into_its_repo() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    let mh = w.state().repos[REPO].merge_head.clone().unwrap();

    let dir = w.queue().dir();
    std::fs::write(
        dir.join("queue.toml"),
        "repo = \"app\"\nmain = \"main\"\nscope = \"trunk\"\n",
    )
    .unwrap();
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap()).unwrap();
    let repos = state.as_object_mut().unwrap().remove("repos").unwrap();
    state["merge_head"] = repos[REPO]["merge_head"].clone();
    state["resolutions"] = serde_json::json!([]);
    std::fs::write(dir.join("state.json"), state.to_string()).unwrap();

    let st = w.state();
    assert_eq!(st.repos[REPO].main, "main");
    assert_eq!(st.repos[REPO].merge_head.as_ref().unwrap().sha, mh.sha);
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    assert_eq!(w.state().sets[&2].deps, [Dep::Set(1)]);
}
