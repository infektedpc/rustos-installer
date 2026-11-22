//! Dependency resolution: pick versions, order installs.
//!
//! `resolve` takes an index and a list of requested dependencies and
//! returns the packages to install, dependencies first (topological
//! order). For each package it picks the newest index version that
//! satisfies *every* accumulated requirement. Failures are errors,
//! never panics:
//!
//! * **unsatisfiable** — no index version meets all requirements on
//!   a package (including plain "package not in the index");
//! * **conflict** — two requirements on one package admit no common
//!   version (reported as unsatisfiable with both requirements shown);
//! * **cycle** — the selected packages depend on each other in a
//!   loop (including a package depending on itself).

use crate::index::{IndexEntry, PackageIndex};
use crate::version::{Dependency, VersionReq};
use std::collections::{HashMap, HashSet};

/// Resolve `requested` against `index`. The returned entries are in
/// install order: every package appears after all of its
/// dependencies.
pub fn resolve(
    index: &PackageIndex,
    requested: &[Dependency],
) -> Result<Vec<IndexEntry>, String> {
    // Constraints only ever grow, and the pick for a package is the
    // newest version satisfying all of them, so re-picking from
    // scratch after each growth converges (versions can only move
    // down, and there are finitely many).
    let mut constraints: HashMap<String, Vec<VersionReq>> = HashMap::new();
    for dep in requested {
        constraints
            .entry(dep.name.clone())
            .or_default()
            .push(dep.req.clone());
    }
    let mut selected: HashMap<String, IndexEntry> = HashMap::new();
    for _ in 0..1000 {
        selected.clear();
        let mut work: Vec<String> = constraints.keys().cloned().collect();
        let mut i = 0;
        while i < work.len() {
            let name = work[i].clone();
            i += 1;
            let reqs = constraints.get(&name).cloned().unwrap_or_default();
            if index.query(&name).is_empty() {
                return Err(format!("package {:?} is not in the index", name));
            }
            let best = index.find_satisfying(&name, &reqs).ok_or_else(|| {
                let reqs_text: Vec<String> = reqs.iter().map(|r| r.to_string()).collect();
                format!(
                    "no version of {:?} satisfies all requirements ({})",
                    name,
                    reqs_text.join(", ")
                )
            })?;
            let entry = best.clone();
            for dep in &entry.depends {
                let slot = constraints.entry(dep.name.clone()).or_default();
                if !slot.contains(&dep.req) {
                    slot.push(dep.req.clone());
                    if !work.contains(&dep.name) {
                        work.push(dep.name.clone());
                    }
                }
            }
            selected.insert(name, entry);
        }
        // Did every pick still satisfy every (possibly grown)
        // constraint? If yes, we have a fixpoint.
        let mut ok = true;
        for (name, entry) in &selected {
            if let Some(reqs) = constraints.get(name) {
                if !reqs.iter().all(|r| r.matches(&entry.version)) {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            check_cycles(&selected)?;
            return topo_sort(&selected);
        }
    }
    Err("dependency resolution did not converge".to_string())
}

/// Report a dependency cycle among the selected packages, naming
/// the loop, e.g. `a -> b -> a`.
fn check_cycles(selected: &HashMap<String, IndexEntry>) -> Result<(), String> {
    // 0 = unvisited, 1 = on the current stack, 2 = done.
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    for name in selected.keys() {
        if state.get(name.as_str()).copied().unwrap_or(0) == 0 {
            visit(name, selected, &mut state, &mut stack)?;
        }
    }
    Ok(())
}

fn visit<'a>(
    name: &'a str,
    selected: &'a HashMap<String, IndexEntry>,
    state: &mut HashMap<&'a str, u8>,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    state.insert(name, 1);
    stack.push(name.to_string());
    if let Some(entry) = selected.get(name) {
        for dep in &entry.depends {
            if !selected.contains_key(&dep.name) {
                continue;
            }
            match state.get(dep.name.as_str()).copied().unwrap_or(0) {
                1 => {
                    // Found the cycle: from the first occurrence of
                    // dep.name on the stack to the top, then back.
                    let start = stack.iter().position(|n| n == &dep.name).unwrap();
                    let mut cycle: Vec<String> = stack[start..].to_vec();
                    cycle.push(dep.name.clone());
                    return Err(format!(
                        "dependency cycle detected: {}",
                        cycle.join(" -> ")
                    ));
                }
                0 => visit(&dep.name, selected, state, stack)?,
                _ => {}
            }
        }
    }
    stack.pop();
    state.insert(name, 2);
    Ok(())
}

/// Order the selected packages so every package comes after its
/// dependencies (Kahn's algorithm over the selected set).
fn topo_sort(selected: &HashMap<String, IndexEntry>) -> Result<Vec<IndexEntry>, String> {
    let mut emitted: HashSet<String> = HashSet::new();
    let mut order: Vec<IndexEntry> = Vec::new();
    let mut remaining: Vec<IndexEntry> = selected.values().cloned().collect();
    // Deterministic output: process names in sorted order.
    remaining.sort_by(|a, b| a.name.cmp(&b.name));
    while !remaining.is_empty() {
        let mut progress = false;
        let mut next: Vec<IndexEntry> = Vec::new();
        for entry in remaining {
            let deps_ready = entry
                .depends
                .iter()
                .all(|d| !selected.contains_key(&d.name) || emitted.contains(&d.name));
            if deps_ready {
                emitted.insert(entry.name.clone());
                order.push(entry);
                progress = true;
            } else {
                next.push(entry);
            }
        }
        if !progress {
            let stuck: Vec<String> = next.iter().map(|e| e.name.clone()).collect();
            return Err(format!(
                "dependency cycle detected among: {}",
                stuck.join(", ")
            ));
        }
        remaining = next;
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;
    use crate::version::{Version, VersionReq};

    fn entry(name: &str, v: (u32, u32, u32), deps: &[(&str, &str)]) -> IndexEntry {
        IndexEntry {
            name: name.to_string(),
            version: Version::new(v.0, v.1, v.2),
            description: String::new(),
            depends: deps
                .iter()
                .map(|(n, r)| Dependency::new(n, VersionReq::parse(r).unwrap()))
                .collect(),
            location: format!("{}.rpk", name),
            crc32: 0,
        }
    }

    fn idx(entries: Vec<IndexEntry>) -> PackageIndex {
        let mut index = PackageIndex::new();
        for e in entries {
            index.add(e);
        }
        index
    }

    fn names(plan: &[IndexEntry]) -> Vec<String> {
        plan.iter().map(|e| format!("{} {}", e.name, e.version)).collect()
    }

    #[test]
    fn diamond_deps_resolve_deps_first() {
        // app -> lib-a, lib-b; both -> lib-core.
        let index = idx(vec![
            entry("app", (1, 0, 0), &[("lib-a", ">=1.0.0"), ("lib-b", ">=1.0.0")]),
            entry("lib-a", (1, 0, 0), &[("lib-core", ">=1.0.0")]),
            entry("lib-b", (1, 0, 0), &[("lib-core", ">=1.0.0")]),
            entry("lib-core", (1, 0, 0), &[]),
        ]);
        let plan = resolve(&index, &[Dependency::new("app", VersionReq::Any)]).unwrap();
        assert_eq!(
            names(&plan),
            vec!["lib-core 1.0.0", "lib-a 1.0.0", "lib-b 1.0.0", "app 1.0.0"]
        );
    }

    #[test]
    fn picks_newest_satisfying_all_constraints() {
        // lib-a wants core >=1.0.0, lib-b wants core <1.5.0: core 1.2.0 wins.
        let index = idx(vec![
            entry("app", (1, 0, 0), &[("lib-a", "*"), ("lib-b", "*")]),
            entry("lib-a", (1, 0, 0), &[("lib-core", ">=1.0.0")]),
            entry("lib-b", (1, 0, 0), &[("lib-core", "<1.5.0")]),
            entry("lib-core", (1, 0, 0), &[]),
            entry("lib-core", (1, 2, 0), &[]),
            entry("lib-core", (1, 9, 0), &[]),
        ]);
        let plan = resolve(&index, &[Dependency::new("app", VersionReq::Any)]).unwrap();
        let core = plan.iter().find(|e| e.name == "lib-core").unwrap();
        assert_eq!(core.version, Version::new(1, 2, 0));
    }

    #[test]
    fn cycle_detection() {
        let index = idx(vec![
            entry("a", (1, 0, 0), &[("b", "*")]),
            entry("b", (1, 0, 0), &[("c", "*")]),
            entry("c", (1, 0, 0), &[("a", "*")]),
        ]);
        let err = resolve(&index, &[Dependency::new("a", VersionReq::Any)]).unwrap_err();
        assert!(err.contains("cycle"), "{}", err);
        // The cycle is reported starting from an arbitrary member of
        // the loop; every link of a -> b -> c -> a must be present.
        assert!(err.contains("a -> b"), "{}", err);
        assert!(err.contains("b -> c"), "{}", err);
        assert!(err.contains("c -> a"), "{}", err);
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let index = idx(vec![entry("a", (1, 0, 0), &[("a", "*")])]);
        let err = resolve(&index, &[Dependency::new("a", VersionReq::Any)]).unwrap_err();
        assert!(err.contains("cycle"), "{}", err);
    }

    #[test]
    fn version_conflict_is_an_error() {
        // lib-a needs core >=2.0.0, lib-b needs core <2.0.0: no overlap.
        let index = idx(vec![
            entry("app", (1, 0, 0), &[("lib-a", "*"), ("lib-b", "*")]),
            entry("lib-a", (1, 0, 0), &[("lib-core", ">=2.0.0")]),
            entry("lib-b", (1, 0, 0), &[("lib-core", "<2.0.0")]),
            entry("lib-core", (1, 0, 0), &[]),
            entry("lib-core", (2, 0, 0), &[]),
        ]);
        let err = resolve(&index, &[Dependency::new("app", VersionReq::Any)]).unwrap_err();
        assert!(err.contains("lib-core"), "{}", err);
        assert!(err.contains("no version"), "{}", err);
    }

    #[test]
    fn unsatisfiable_and_missing_package() {
        let index = idx(vec![entry("a", (1, 0, 0), &[("b", ">=9.0.0")]), entry("b", (1, 0, 0), &[])]);
        let err = resolve(&index, &[Dependency::new("a", VersionReq::Any)]).unwrap_err();
        assert!(err.contains("b"), "{}", err);
        let err = resolve(&index, &[Dependency::new("ghost", VersionReq::Any)]).unwrap_err();
        assert!(err.contains("ghost"), "{}", err);
        assert!(err.contains("not in the index"), "{}", err);
    }

    #[test]
    fn requested_version_requirement_is_honoured() {
        let index = idx(vec![entry("a", (1, 0, 0), &[]), entry("a", (2, 0, 0), &[])]);
        let plan = resolve(&index, &[Dependency::new("a", VersionReq::parse("=1.0.0").unwrap())]).unwrap();
        assert_eq!(names(&plan), vec!["a 1.0.0"]);
        let plan = resolve(&index, &[Dependency::new("a", VersionReq::Any)]).unwrap();
        assert_eq!(names(&plan), vec!["a 2.0.0"]);
    }
}
