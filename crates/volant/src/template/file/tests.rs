// SPDX-License-Identifier: GPL-3.0-or-later
use crate::testing::{scratch, templar};
use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::*;

/// A managed host's value lands in the file as text and is never rendered again.
#[test]
fn a_host_value_in_a_template_is_written_as_text() {
    let marker_guard = scratch();
    let marker = marker_guard.path().join("marker");
    let payload = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
    let mut map = Map::new();
    map.insert("motd".into(), Value::String(payload.clone()));
    let untrusted: BTreeSet<String> = ["motd".to_string()].into();
    let vars = Vars {
        map: &map,
        hostvars: None,
        shared: None,
        facts: None,
        untrusted: Some(&untrusted),
        untrusted_hosts: None,
    };
    let out = templar()
        .render_file("banner: {{ motd }}\n", vars, &FileRender::default())
        .unwrap();
    assert_eq!(out, format!("banner: {payload}\n"));
    assert!(
        !marker.exists(),
        "the host's template ran on the controller"
    );
}

/// Even an author value is not rendered twice: `{% raw %}` writes its braces, measured on
/// ansible-core 2.19.12 (`{{ literal }}` in the rendered file).
#[test]
fn a_rendered_file_is_never_rendered_again() {
    let out = templar()
        .render_file(
            "{% raw %}{{ literal }}{% endraw %}\n",
            &Map::new(),
            &FileRender::default(),
        )
        .unwrap();
    assert_eq!(out, "{{ literal }}\n");
}

const OPTS: &str = "top\n  {% if true %}\n  in\n  {% endif %}\nend\n";

/// The four shapes measured on the reference with this very template.
#[test]
fn the_template_module_s_options_are_the_reference_s() {
    let r = |o: FileRender| templar().render_file(OPTS, &Map::new(), &o).unwrap();
    assert_eq!(r(FileRender::default()), "top\n    in\n  end\n");
    assert_eq!(
        r(FileRender {
            lstrip_blocks: true,
            ..Default::default()
        }),
        "top\n  in\nend\n"
    );
    assert_eq!(
        r(FileRender {
            trim_blocks: false,
            ..Default::default()
        }),
        "top\n  \n  in\n  \nend\n"
    );
    assert_eq!(
        r(FileRender {
            newline_sequence: "\r\n".into(),
            ..Default::default()
        }),
        "top\r\n    in\r\n  end\r\n"
    );
}

/// The final newline of the file survives, and a template that renders to nothing but it writes
/// it: measured, a role's `sshd_config_snippet.j2` with its `if` false is one byte, `\n`.
#[test]
fn the_final_newline_is_kept() {
    let r = |t: &str| {
        templar()
            .render_file(t, &Map::new(), &FileRender::default())
            .unwrap()
    };
    assert_eq!(r("{% if false %}x{% endif %}\n"), "\n");
    assert_eq!(r("a"), "a");
    assert_eq!(r("a\n\n"), "a\n\n");
    assert_eq!(r("\n\n"), "\n\n");
    assert_eq!(r("{% if true %}x{% endif %}\n\n"), "x\n\n");
    assert_eq!(r("{% raw %}a\n{% endraw %}\n\n"), "a\n\n");
}

/// Jinja2's lexer rewrites every `\r\n` and lone `\r` in the source to `\n` before it ever sees
/// the template; minijinja does not.
#[test]
fn the_source_s_own_crlf_is_normalised_like_the_reference() {
    assert_eq!(
        templar()
            .render_file("x\r\ny\r\n", &Map::new(), &FileRender::default())
            .unwrap(),
        "x\ny\n"
    );
    assert_eq!(
        templar()
            .render_file(
                "x\r\ny\r\n",
                &Map::new(),
                &FileRender {
                    newline_sequence: "\r\n".into(),
                    ..Default::default()
                }
            )
            .unwrap(),
        "x\r\ny\r\n"
    );
    // The reference counts trailing newlines on the raw text, not the normalised copy: a
    // trailing `\r\n\r\n` is one `\n` run broken by a `\r`, not two.
    assert_eq!(
        templar()
            .render_file("x\r\n\r\n", &Map::new(), &FileRender::default())
            .unwrap(),
        "x\n"
    );
}
