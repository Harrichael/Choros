//! A queue's open sets drawn as `git log --graph` draws commits: each set
//! above the sets it lands after, with a lane down to each of them. Sets
//! after nothing in the queue run down to main, drawn last, so sets that
//! don't touch read as side by side, not as a chain.

use std::collections::BTreeMap;

pub enum Row {
    /// Lanes only, as they merge or split between two sets.
    Edges(String),
    /// Set `n`. `graph` goes before its line; `under` before each line that
    /// says more about it.
    Set {
        n: u32,
        graph: String,
        under: String,
    },
    /// Main, beneath everything.
    Main(String),
}

const MAIN: u32 = u32::MAX;

/// `sets` maps each set to those it lands after; ones not in `sets` are
/// left out. Sets come out dependents first, newest first among the free.
pub fn draw(sets: &BTreeMap<u32, Vec<u32>>) -> Vec<Row> {
    if sets.is_empty() {
        return Vec::new();
    }
    let mut parents: BTreeMap<u32, Vec<u32>> = sets
        .iter()
        .map(|(n, deps)| {
            let mut ps: Vec<u32> = deps
                .iter()
                .copied()
                .filter(|d| sets.contains_key(d) && d != n)
                .collect();
            ps.sort_unstable_by(|a, b| b.cmp(a));
            ps.dedup();
            if ps.is_empty() {
                ps.push(MAIN);
            }
            (*n, ps)
        })
        .collect();
    parents.insert(MAIN, Vec::new());
    let mut rows = Vec::new();
    let mut lanes: Vec<Option<u32>> = Vec::new();
    for n in dependents_first(&parents) {
        let ps = &parents[&n];
        if !lanes.contains(&Some(n)) {
            lanes.push(Some(n));
        }

        // Every lane heading to `n` joins the first.
        let c = lanes.iter().position(|l| *l == Some(n)).unwrap();
        let mut rank = 0;
        let targets: Vec<usize> = lanes
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i > c && *l == Some(n) {
                    return c;
                }
                rank += 1;
                rank - 1
            })
            .collect();
        shift(&mut lanes, &targets, &mut rows);

        // A set after several others fans out as git's octopus merges do:
        // lanes to its right make room first, so the fan crosses nothing.
        let k = ps.len().saturating_sub(1);
        if k >= 2 {
            let targets: Vec<usize> = (0..lanes.len())
                .map(|i| if i > c { i + k - 1 } else { i })
                .collect();
            shift(&mut lanes, &targets, &mut rows);
        }

        let width = 2 * (lanes.len() + k);
        let mut graph = lane_cells(&lanes);
        graph.insert(2 * c, '*');
        if k >= 2 {
            for x in 2 * c + 1..2 * c + 2 * k - 2 {
                graph.insert(x, '-');
            }
            graph.insert(2 * c + 2 * k - 2, '.');
        }
        let mut under = lane_cells(&lanes);
        if ps.is_empty() {
            under.remove(&(2 * c));
        }
        rows.push(match n {
            MAIN => Row::Main(render(&graph, width)),
            n => Row::Set {
                n,
                graph: render(&graph, width),
                under: render(&under, width),
            },
        });

        if ps.is_empty() {
            lanes[c] = None;
            let targets: Vec<usize> = (0..lanes.len())
                .map(|i| if i > c { i - 1 } else { i })
                .collect();
            shift(&mut lanes, &targets, &mut rows);
            continue;
        }
        lanes[c] = Some(ps[0]);
        if k >= 1 {
            let mut cells = lane_cells(&lanes[..=c]);
            for j in 0..k {
                cells.insert(2 * c + 1 + 2 * j, '\\');
            }
            let right: Vec<(usize, u32)> = lanes
                .iter()
                .enumerate()
                .skip(c + 1)
                .filter_map(|(i, l)| l.map(|l| (i, l)))
                .collect();
            for (i, _) in &right {
                cells.insert(2 * i + 1, '\\');
            }
            rows.push(Row::Edges(render(&cells, 0)));
            let mut next: Vec<Option<u32>> = lanes[..=c].to_vec();
            next.extend(ps[1..].iter().map(|p| Some(*p)));
            next.resize(right.last().map_or(next.len(), |(i, _)| i + 2), None);
            for (i, l) in right {
                next[i + 1] = Some(l);
            }
            lanes = next;
        }
    }
    rows
}

/// Each set once all that land after it are out; the newest free one first.
fn dependents_first(parents: &BTreeMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut waiting: BTreeMap<u32, usize> = parents.keys().map(|n| (*n, 0)).collect();
    for ps in parents.values() {
        for p in ps {
            *waiting.get_mut(p).unwrap() += 1;
        }
    }
    let mut out = Vec::new();
    while let Some(n) = waiting
        .iter()
        .rev()
        .find(|(_, w)| **w == 0)
        .map(|(n, _)| *n)
    {
        waiting.remove(&n);
        for p in &parents[&n] {
            *waiting.get_mut(p).unwrap() -= 1;
        }
        out.push(n);
    }
    out
}

/// Moves each lane to its target column, one column a row, drawing the rows.
fn shift(lanes: &mut Vec<Option<u32>>, targets: &[usize], rows: &mut Vec<Row>) {
    let mut at: Vec<usize> = (0..lanes.len()).collect();
    loop {
        let mut cells = BTreeMap::new();
        let mut moved = false;
        for (i, l) in lanes.iter().enumerate() {
            if l.is_none() {
                continue;
            }
            let (p, t) = (at[i], targets[i]);
            if p < t {
                cells.insert(2 * p + 1, '\\');
                at[i] += 1;
            } else if p > t {
                cells.insert(2 * p - 1, '/');
                at[i] -= 1;
            } else {
                cells.insert(2 * p, '|');
                continue;
            }
            moved = true;
        }
        if !moved {
            break;
        }
        rows.push(Row::Edges(render(&cells, 0)));
    }
    let mut next = vec![
        None;
        lanes
            .iter()
            .zip(targets)
            .filter(|(l, _)| l.is_some())
            .map(|(_, t)| t + 1)
            .max()
            .unwrap_or(0)
    ];
    for (l, t) in lanes.iter().zip(targets) {
        if l.is_some() {
            next[*t] = *l;
        }
    }
    *lanes = next;
}

fn lane_cells(lanes: &[Option<u32>]) -> BTreeMap<usize, char> {
    lanes
        .iter()
        .enumerate()
        .filter(|(_, l)| l.is_some())
        .map(|(i, _)| (2 * i, '|'))
        .collect()
}

/// The cells as a line, padded to `width`; with no width, trimmed.
fn render(cells: &BTreeMap<usize, char>, width: usize) -> String {
    let len = cells.keys().next_back().map_or(0, |x| x + 1).max(width);
    let mut line = vec![' '; len];
    for (x, ch) in cells {
        line[*x] = *ch;
    }
    let line: String = line.into_iter().collect();
    if width == 0 {
        line.trim_end().to_string()
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(sets: &[(u32, &[u32])]) -> String {
        let sets = sets.iter().map(|(n, d)| (*n, d.to_vec())).collect();
        draw(&sets)
            .into_iter()
            .map(|r| match r {
                Row::Edges(g) => g,
                Row::Set { n, graph, .. } => format!("{graph}{n}"),
                Row::Main(graph) => format!("{graph}main"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A chain is a single lane; two sets after one fan out from it and join
    /// back where they meet, as git draws a merge. Sets after nothing in
    /// the queue each run down to main.
    #[test]
    fn sets_draw_as_git_draws_commits() {
        assert_eq!(
            picture(&[(1, &[]), (2, &[1]), (3, &[2])]),
            "* 3\n* 2\n* 1\n* main"
        );
        assert_eq!(
            picture(&[(1, &[]), (2, &[1]), (3, &[1]), (4, &[2, 3])]),
            ["*   4", "|\\", "* | 3", "| * 2", "|/", "* 1", "* main"].join("\n")
        );
        assert_eq!(
            picture(&[(1, &[]), (2, &[]), (3, &[])]),
            ["* 3", "| * 2", "| | * 1", "|/ /", "|/", "* main"].join("\n")
        );
        assert!(picture(&[]).is_empty());
    }

    /// A set after three others fans out without crossing the lane beside
    /// it, and a lane joining one far to its left steps over those between.
    #[test]
    fn wide_fans_and_long_joins_cross_nothing() {
        assert_eq!(
            picture(&[(1, &[]), (2, &[]), (3, &[]), (4, &[1, 2, 3]), (5, &[])]),
            [
                "* 5",
                "| *-.   4",
                "| |\\ \\",
                "| * | | 3",
                "| | * | 2",
                "| | | * 1",
                "|/ / /",
                "|/ /",
                "|/",
                "* main"
            ]
            .join("\n")
        );
        assert_eq!(
            picture(&[
                (1, &[]),
                (2, &[1]),
                (3, &[]),
                (4, &[3, 1]),
                (5, &[2]),
                (6, &[1])
            ]),
            [
                "* 6",
                "| * 5",
                "| | *   4",
                "| | |\\",
                "| | * | 3",
                "| * | | 2",
                "|/ / /",
                "| |/",
                "|/|",
                "* | 1",
                "|/",
                "* main",
            ]
            .join("\n")
        );
    }
}
