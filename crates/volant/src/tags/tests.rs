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
