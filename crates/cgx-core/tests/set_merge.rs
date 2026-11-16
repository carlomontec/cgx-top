use cgx_core::*;
use std::fs;
use std::path::PathBuf;

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cgx_set_merge_{}_{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn nam_and_inp_sets_are_merged_without_duplicates() {
    let dir = tmp_dir("nam_inp");
    fs::write(dir.join("job.nam"), "*NSET, NSET=NFIX\n5, 3, 1\n*ELSET, ELSET=EALL\n10, 11\n").unwrap();
    fs::write(
        dir.join("job.inp"),
        "*NODE\n1, 0, 0, 0\n*NSET, NSET=NFIX\n3, 1, 8, 5\n*ELSET, ELSET=EALL\n11, 12\n*INCLUDE, INPUT=job.nam\n",
    )
    .unwrap();
    let frd = dir.join("job.frd");

    // Companion loading reads job.inp (which also includes job.nam) and the stray job.nam.
    let sets = load_companion_sets(&frd);
    let nfix = sets.get("NFIX").unwrap();
    assert_eq!(nfix.node_ids.len(), 4);
    let mut sorted = nfix.node_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, vec![1, 3, 5, 8]);
    // First-seen order: the deck's own definition comes first.
    assert_eq!(nfix.node_ids, vec![3, 1, 8, 5]);
    assert_eq!(sets.get("EALL").unwrap().element_ids, vec![11, 12, 10]);

    // Same through the model, loading the two files one after the other.
    let mut model = FrdModel::new();
    model.load_inp_sets(dir.join("job.nam")).unwrap();
    model.load_inp_sets(dir.join("job.inp")).unwrap();
    model.load_companion_sets(&frd);
    assert_eq!(model.sets["NFIX"].node_ids, vec![5, 3, 1, 8]);
    assert_eq!(model.sets["EALL"].element_ids, vec![10, 11, 12]);
}

#[test]
fn repeated_definition_in_one_deck_is_deduplicated() {
    let sets = parse_sets_from_str("*NSET, NSET=A\n1, 2, 2\n*NSET, NSET=A, GENERATE\n2, 4\n");
    assert_eq!(sets["A"].node_ids, vec![1, 2, 3, 4]);
}
