use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DagError {
    DuplicateNode(String),
    MissingDependency { node: String, dependency: String },
    RepeatedDependency { node: String, dependency: String },
    Cycle(Vec<String>),
}

pub fn insert_node<'a>(indexed: &mut HashMap<&'a str, usize>, id: &'a str) -> Result<(), DagError> {
    let index = indexed.len();
    if indexed.insert(id, index).is_some() {
        return Err(DagError::DuplicateNode(id.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dag_accepts_shared_predecessors_and_rejects_a_real_back_edge() {
        let dependencies = [
            vec![],
            vec!["a".into()],
            vec!["a".into()],
            vec!["b".into(), "c".into()],
        ];
        let names = ["a", "b", "c", "d"];
        let mut ids = HashMap::new();
        for name in names {
            insert_node(&mut ids, name).unwrap();
        }
        assert!(
            validate_dependencies(
                &ids,
                names
                    .into_iter()
                    .zip(dependencies.iter().map(Vec::as_slice))
            )
            .is_ok()
        );
        let mut cycle = dependencies;
        cycle[0] = vec!["d".into()];
        assert_eq!(
            validate_dependencies(&ids, names.into_iter().zip(cycle.iter().map(Vec::as_slice)))
                .unwrap_err(),
            DagError::Cycle(vec!["a".into(), "d".into(), "b".into(), "a".into()])
        );
        assert!(validate_dependencies(&HashMap::new(), std::iter::empty()).is_ok());
    }
}

pub fn validate_dependencies<'a>(
    ids: &HashMap<&str, usize>,
    nodes: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> Result<(), DagError> {
    let mut names = vec![""; ids.len()];
    let mut edges = vec![Vec::new(); ids.len()];
    for (id, dependencies) in nodes {
        let index = ids[id];
        names[index] = id;
        let mut seen = HashSet::new();
        for dependency in dependencies {
            let Some(&next) = ids.get(dependency.as_str()) else {
                return Err(DagError::MissingDependency {
                    node: id.into(),
                    dependency: dependency.clone(),
                });
            };
            if !seen.insert(next) {
                return Err(DagError::RepeatedDependency {
                    node: id.into(),
                    dependency: dependency.clone(),
                });
            }
            edges[index].push(next);
        }
    }
    // Iterative DFS returns the actual back-edge path without risking recursion on long DAGs.
    let mut color = vec![0; ids.len()];
    for root in 0..ids.len() {
        if color[root] != 0 {
            continue;
        }
        color[root] = 1;
        let mut stack = vec![(root, 0)];
        while let Some((node, edge)) = stack.last_mut() {
            let Some(&next) = edges[*node].get(*edge) else {
                color[*node] = 2;
                stack.pop();
                continue;
            };
            *edge += 1;
            if color[next] == 1 {
                let start = stack.iter().position(|(id, _)| *id == next).unwrap_or(0);
                let path = stack[start..]
                    .iter()
                    .map(|(id, _)| names[*id].to_owned())
                    .chain(std::iter::once(names[next].to_owned()))
                    .collect();
                return Err(DagError::Cycle(path));
            }
            if color[next] == 0 {
                color[next] = 1;
                stack.push((next, 0));
            }
        }
    }
    Ok(())
}
