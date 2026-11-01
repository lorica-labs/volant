// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use serde_json::json;

fn trusted_at(line: u32) -> TagSetId {
    intern(TagSet {
        origin: Some(origin_at("/play.yml", line, 3)),
        trusted: true,
        ..TagSet::default()
    })
}

#[test]
fn interning_gives_one_id_per_distinct_set() {
    assert_eq!(trusted_at(1), trusted_at(1));
    assert_ne!(trusted_at(1), trusted_at(2));
    assert_eq!(intern(TagSet::default()), TagSetId::NONE);
    assert_eq!(
        get(trusted_at(1)).origin,
        Some(origin_at("/play.yml", 1, 3))
    );
}

#[test]
fn an_origin_reads_as_ansible_core_prints_it() {
    assert_eq!(origin_text(origin_at("/play.yml", 4, 7)), "/play.yml:4:7");
    assert_eq!(origin_text(origin_at("/play.yml", 4, 0)), "/play.yml:4");
    assert_eq!(origin_text(origin_at("/play.yml", 0, 0)), "/play.yml");
    let described = origin_described("<environment variable 'HOME'>");
    assert_eq!(origin_text(described), "<environment variable 'HOME'>");
    assert_eq!(origin_place(described), None);
}

#[test]
fn names_come_sorted_as_tag_names_sorts_them() {
    let every = intern(TagSet {
        origin: Some(origin_described("<x>")),
        trusted: true,
        source_encrypted: true,
        vaulted: Some(Arc::from("$ANSIBLE_VAULT;1.1;AES256")),
        deprecated: Some(Arc::new(Deprecated {
            msg: "old".into(),
            help_text: None,
            date: None,
            version: None,
            deprecator: None,
        })),
    });
    assert_eq!(
        names(every),
        [
            "Deprecated",
            "Origin",
            "SourceWasEncrypted",
            "TrustedAsTemplate",
            "VaultedValue"
        ]
    );
    assert!(names(TagSetId::NONE).is_empty());
}

fn map(entries: Vec<(&str, TagTree)>) -> TagTree {
    let entries = entries
        .into_iter()
        .map(|(k, t)| (k.to_string(), t))
        .collect();
    TagTree::Node(TagSetId::NONE, Children::Map(entries))
}

#[test]
fn a_merge_keeps_each_leaf_s_tags_and_the_new_side_wins_a_shared_key() {
    let old = json!({"a": {"x": 1, "y": 2}, "b": 3});
    let new = json!({"a": {"y": 20, "z": 30}, "c": 4});
    let mine = map(vec![
        (
            "a",
            map(vec![
                ("x", TagTree::Leaf(trusted_at(10))),
                ("y", TagTree::Leaf(trusted_at(11))),
            ]),
        ),
        ("b", TagTree::Leaf(trusted_at(12))),
    ]);
    let theirs = map(vec![
        ("a", map(vec![("z", TagTree::Leaf(trusted_at(21)))])),
        ("c", TagTree::Leaf(trusted_at(22))),
    ]);
    let merged = merge(Some(&mine), Some(&theirs), &old, &new).unwrap();
    let a = merged.at("a").unwrap();
    assert_eq!(a.at("x").map(TagTree::own), Some(trusted_at(10)));
    // `y` came from `new`, whose `y` has no tags: the old tags must not stay.
    assert_eq!(a.at("y"), None);
    assert_eq!(a.at("z").map(TagTree::own), Some(trusted_at(21)));
    assert_eq!(merged.at("b").map(TagTree::own), Some(trusted_at(12)));
    assert_eq!(merged.at("c").map(TagTree::own), Some(trusted_at(22)));
}

#[test]
fn a_value_replaced_by_a_merge_takes_the_new_tree_or_none() {
    let mine = TagTree::Leaf(trusted_at(30));
    assert_eq!(merge(Some(&mine), None, &json!("old"), &json!("new")), None);
    let theirs = TagTree::Leaf(trusted_at(31));
    let merged = merge(Some(&mine), Some(&theirs), &json!({"k": 1}), &json!([1]));
    assert_eq!(merged, Some(theirs));
}

#[test]
fn a_node_without_tags_is_no_tree() {
    assert_eq!(TagTree::node(TagSetId::NONE, Children::Seq(vec![])), None);
    let seq = TagTree::node(
        TagSetId::NONE,
        Children::Seq(vec![(2, TagTree::Leaf(trusted_at(40)))]),
    );
    assert_eq!(
        seq.unwrap().index(2).map(TagTree::own),
        Some(trusted_at(40))
    );
}

fn line_of(tree: Option<&TagTree>) -> Option<u32> {
    let origin = get(tree?.own()).origin?;
    origin_place(origin).map(|(_, line, _)| line)
}

#[test]
fn a_value_written_without_its_tree_drops_the_old_one() {
    let mut layer = Tagged::default();
    layer.insert(
        "x".into(),
        json!("{{ a }}"),
        Some(TagTree::Leaf(trusted_at(50))),
    );
    layer.insert("x".into(), json!("{{ a }}"), None);
    assert_eq!(layer.tree("x"), None);
    layer.insert("y".into(), json!(1), Some(TagTree::Leaf(trusted_at(51))));
    layer.overlay(&Tagged::from(json!({"y": 2}).as_object().unwrap().clone()));
    assert_eq!(layer.tree("y"), None);
}

#[test]
fn a_hash_merge_keeps_the_origins_of_both_sides() {
    crate::vars::set_hash_merge(true);
    let tree = |key: &str, line| map(vec![(key, TagTree::Leaf(trusted_at(line)))]);
    let mut low = Tagged::default();
    low.insert("x".into(), json!({"a": 1}), Some(tree("a", 60)));
    let mut high = Tagged::default();
    high.insert("x".into(), json!({"b": 2}), Some(tree("b", 61)));
    low.extend(&high);
    crate::vars::set_hash_merge(false);
    assert_eq!(low.get("x"), Some(&json!({"a": 1, "b": 2})));
    let x = low.tree("x");
    assert_eq!(line_of(x.and_then(|t| t.at("a"))), Some(60));
    assert_eq!(line_of(x.and_then(|t| t.at("b"))), Some(61));
}

#[test]
fn loaded_strings_are_trusted_and_unsafe_ones_are_not() {
    let text = "a: text\nb: !unsafe '{{ x }}'\nn: 3\n";
    let (docs, origins) = crate::yaml::load_with_origins(text, "t.yml").unwrap();
    let tree = crate::yaml::tags_of(&docs[0], &origins, Some("/t.yml"), true).unwrap();
    let a = get(tree.at("a").unwrap().own());
    assert!(a.trusted);
    assert_eq!(line_of(tree.at("a")), Some(1));
    let b = get(tree.at("b").unwrap().own());
    assert!(!b.trusted);
    assert_eq!(line_of(tree.at("b")), Some(2));
    assert!(!get(tree.at("n").unwrap().own()).trusted);
}

#[test]
fn an_extra_variable_comes_from_the_cli_option_and_is_trusted() {
    let vars = crate::vars::parse_extra_vars(&["k=v".into()], std::path::Path::new(".")).unwrap();
    let set = get(vars.tree("k").unwrap().own());
    assert!(set.trusted);
    assert_eq!(
        origin_text(set.origin.unwrap()),
        "<CLI option 'extra_vars'>"
    );
}
