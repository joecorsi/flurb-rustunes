//! Three-way playlist merging. No file, database or transport access.
use super::*;

pub(super) fn merge(
    base: &Option<PlaylistState>,
    local: &Option<PlaylistState>,
    phone: &Option<PlaylistState>,
) -> std::result::Result<Option<PlaylistState>, String> {
    if local == phone {
        return Ok(local.clone());
    }
    if local == base {
        return Ok(phone.clone());
    }
    if phone == base {
        return Ok(local.clone());
    }
    let (Some(base), Some(local), Some(phone)) = (base, local, phone) else {
        return Err("One side deleted the playlist while the other edited it.".into());
    };
    let name = if local.name == phone.name || phone.name == base.name {
        local.name.clone()
    } else if local.name == base.name {
        phone.name.clone()
    } else {
        return Err("The playlist was renamed differently on both sides.".into());
    };
    let tracks = merge_order(&base.tracks, &local.tracks, &phone.tracks)?;
    Ok(Some(PlaylistState { name, tracks }))
}
fn positions(ids: &[String]) -> BTreeMap<&str, usize> {
    ids.iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect()
}
fn relation(positions: &BTreeMap<&str, usize>, a: &str, b: &str) -> Option<bool> {
    Some(positions.get(a)? < positions.get(b)?)
}
fn merge_order(
    base: &[String],
    local: &[String],
    phone: &[String],
) -> std::result::Result<Vec<String>, String> {
    let b = positions(base);
    let l = positions(local);
    let p = positions(phone);
    if b.len() != base.len() || l.len() != local.len() || p.len() != phone.len() {
        return Err("Repeated tracks cannot be merged safely.".into());
    }
    let ids: Vec<_> = l
        .keys()
        .chain(p.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| !b.contains_key(id) || (l.contains_key(id) && p.contains_key(id)))
        .collect();
    if ids.len() > 499 {
        return Err("The merged playlist exceeds the current 499-track limit.".into());
    }
    let mut edges = vec![Vec::new(); ids.len()];
    let mut incoming = vec![0; ids.len()];
    for a in 0..ids.len() {
        for z in a + 1..ids.len() {
            let old = relation(&b, ids[a], ids[z]);
            let left = relation(&l, ids[a], ids[z]);
            let right = relation(&p, ids[a], ids[z]);
            let order = match (left, right) {
                (Some(x), Some(y)) if x == y => Some(x),
                (Some(x), Some(y)) if Some(x) == old => Some(y),
                (Some(x), Some(y)) if Some(y) == old => Some(x),
                (Some(_), Some(_)) => {
                    return Err("Tracks were reordered incompatibly on both sides.".into());
                }
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
            if let Some(a_first) = order {
                let (from, to) = if a_first { (a, z) } else { (z, a) };
                edges[from].push(to);
                incoming[to] += 1;
            }
        }
    }
    let rank = |i: usize| {
        (
            *b.get(ids[i]).unwrap_or(&usize::MAX),
            *l.get(ids[i]).unwrap_or(&usize::MAX),
            *p.get(ids[i]).unwrap_or(&usize::MAX),
            ids[i],
            i,
        )
    };
    let mut ready: BTreeSet<_> = (0..ids.len())
        .filter(|i| incoming[*i] == 0)
        .map(rank)
        .collect();
    let mut result = Vec::with_capacity(ids.len());
    while let Some((_, _, _, id, index)) = ready.pop_first() {
        result.push(id.to_owned());
        for next in &edges[index] {
            incoming[*next] -= 1;
            if incoming[*next] == 0 {
                ready.insert(rank(*next));
            }
        }
    }
    if result.len() != ids.len() {
        return Err("The combined track ordering is contradictory.".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(name: &str, tracks: &[&str]) -> Option<PlaylistState> {
        Some(PlaylistState {
            name: name.into(),
            tracks: tracks.iter().map(|v| (*v).into()).collect(),
        })
    }
    #[test]
    fn one_sided_changes_deletions_and_matching_edits_flow_without_conflict() {
        let b = state("Mix", &["a", "b"]);
        let changed = state("Renamed", &["b", "a"]);
        assert_eq!(merge(&b, &b, &changed).unwrap(), changed);
        assert_eq!(merge(&b, &changed, &b).unwrap(), changed);
        assert_eq!(merge(&b, &changed, &changed).unwrap(), changed);
        assert_eq!(merge(&b, &b, &None).unwrap(), None);
        assert!(merge(&b, &changed, &None).is_err());
    }
    #[test]
    fn independent_renames_additions_and_removals_merge_in_order() {
        let b = state("Mix", &["a", "b", "c"]);
        let l = state("New name", &["a", "b", "c", "d"]);
        let p = state("Mix", &["a", "c", "e"]);
        assert_eq!(
            merge(&b, &l, &p).unwrap(),
            state("New name", &["a", "c", "d", "e"])
        );
    }
    #[test]
    fn reorders_and_compatible_insertions_are_preserved_without_duplicates() {
        let b = state("Mix", &["a", "b", "c"]);
        let l = state("Mix", &["b", "a", "c"]);
        let p = state("Mix", &["a", "b", "c", "d"]);
        assert_eq!(
            merge(&b, &l, &p).unwrap(),
            state("Mix", &["b", "a", "c", "d"])
        );
        assert_eq!(
            merge(
                &b,
                &state("Mix", &["a", "x", "b", "c"]),
                &state("Mix", &["a", "x", "b", "c"])
            )
            .unwrap(),
            state("Mix", &["a", "x", "b", "c"])
        );
    }
    #[test]
    fn competing_names_and_cyclic_order_edits_are_conflicts() {
        let b = state("Mix", &["a", "b", "c"]);
        assert!(
            merge(
                &b,
                &state("One", &["a", "b", "c"]),
                &state("Two", &["a", "b", "c"])
            )
            .is_err()
        );
        assert!(
            merge(
                &b,
                &state("Mix", &["b", "a", "c"]),
                &state("Mix", &["a", "c", "b"])
            )
            .is_err()
        );
    }
}
