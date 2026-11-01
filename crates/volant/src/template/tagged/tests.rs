// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use rstest::rstest;
use serde_json::{Map, Value, json};

use crate::tags::{OriginId, TagMap, TagSet, TagSetId, TagTree, intern, origin_described};
use crate::template::Vars;
use crate::testing::templar;

fn set(origin: &str, trusted: bool) -> TagSetId {
    intern(TagSet {
        origin: Some(origin_described(origin)),
        trusted,
        ..TagSet::default()
    })
}

/// The written variables: `text` and `list` tagged as loaded from `vars.yml`, and two sevens
/// carrying different tags.
fn written() -> (Map<String, Value>, Arc<TagMap>) {
    let vars = json!({"text": "hi", "list": [1, 2], "seven": 7, "other_seven": 7});
    let tags = TagMap::from_iter([
        ("text".into(), TagTree::Leaf(set("vars.yml", true))),
        ("list".into(), TagTree::Leaf(set("vars.yml", false))),
        ("seven".into(), TagTree::Leaf(set("vars.yml", false))),
        ("other_seven".into(), TagTree::Leaf(set("other.yml", false))),
    ]);
    (vars.as_object().unwrap().clone(), Arc::new(tags))
}

/// The tags of what `template` renders to as the task argument `msg`, written in `task.yml`.
fn tags_of(template: &str) -> TagSetId {
    let (map, tags) = written();
    let vars = Vars {
        tags: Some(&tags),
        ..Vars::from(&map)
    };
    let task = origin_described("task.yml");
    let origin_of = |_: &str| -> Option<OriginId> { Some(task) };
    let args = json!({"msg": template});
    let (_, _, out) = templar()
        .render_map_tagged(args.as_object().unwrap(), vars, &origin_of)
        .unwrap();
    out.get("msg").map_or(TagSetId::NONE, TagTree::own)
}

/// A value passed through keeps its tags. A computed value, or a number two variables share
/// with different tags, has the template's origin and no trust; text as written has both.
#[rstest]
#[case::passed_through("{{ text }}", set("vars.yml", true))]
#[case::a_container("{{ list }}", set("vars.yml", false))]
#[case::through_a_filter_that_returns_it("{{ text | default('x') }}", set("vars.yml", true))]
#[case::an_operation("{{ text ~ '!' }}", set("task.yml", false))]
#[case::a_new_container("{{ list | list }}", set("task.yml", false))]
#[case::a_number_read_alone("{{ seven }}", set("vars.yml", false))]
#[case::an_ambiguous_number("{{ [seven, other_seven][0] }}", set("task.yml", false))]
#[case::text_as_written("hi", set("task.yml", true))]
fn a_render_keeps_the_tags_of_what_it_passes_through(
    #[case] template: &str,
    #[case] expected: TagSetId,
) {
    assert_eq!(tags_of(template), expected);
}

/// The `ansible._protomatter` plugins read the tags the render knows.
#[rstest]
#[case::tag_names("{{ text | ansible._protomatter.tag_names }}", json!(["Origin", "TrustedAsTemplate"]))]
#[case::origin("{{ text | ansible._protomatter.origin }}", json!("vars.yml"))]
#[case::tagged("{{ text is ansible._protomatter.tagged }}", json!(true))]
#[case::untagged("{{ (text ~ '') is ansible._protomatter.tagged }}", json!(false))]
#[case::tagged_with("{{ list is ansible._protomatter.tagged_with('Origin') }}", json!(true))]
#[case::true_type("{{ text | ansible._protomatter.true_type }}", json!("_AnsibleTaggedStr"))]
#[case::apply_trust("{{ list[0] | string | ansible._protomatter.apply_trust | ansible._protomatter.tag_names }}", json!(["TrustedAsTemplate"]))]
#[case::undefined("{{ (nothing | ansible._protomatter.origin) is undefined }}", json!(true))]
fn the_protomatter_plugins_read_the_tags(#[case] template: &str, #[case] expected: Value) {
    let (map, tags) = written();
    let vars = Vars {
        tags: Some(&tags),
        ..Vars::from(&map)
    };
    assert_eq!(templar().render(template, vars).unwrap(), expected);
}
