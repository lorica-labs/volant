// SPDX-License-Identifier: GPL-3.0-or-later
//! `Templar::render_file`: the one-pass render the `template` module uses. A template's own text
//! is author content read on the controller; a value it reads from a managed host is inserted as
//! text, and the result is never itself rendered, unlike `Templar::render_in`'s extra passes over
//! a task's arguments.

use super::{Templar, TemplateError, Vars, context_of, convert_error};

/// The `template` module's own options, as the reference reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRender {
    /// `trim_blocks`, true unless the task says otherwise.
    pub trim_blocks: bool,
    /// `lstrip_blocks`, false unless the task says otherwise.
    pub lstrip_blocks: bool,
    /// `newline_sequence`, one of "\n", "\r", "\r\n" once the escaped spellings are read.
    pub newline_sequence: String,
}

impl Default for FileRender {
    fn default() -> Self {
        Self {
            trim_blocks: true,
            lstrip_blocks: false,
            newline_sequence: "\n".to_string(),
        }
    }
}

impl Templar {
    /// Renders a template file's text **once**, as the `template` module does: `render_str`, not
    /// `render_in`'s further passes, so a value read from a managed host lands in the output as
    /// text and is never rendered a second time. A dedicated `Environment`, cloned from the
    /// `Templar`'s own so it keeps every filter, test and lookup the engine has, carries the
    /// file's own `trim_blocks`/`lstrip_blocks`; a render per task does not make cloning it worth
    /// caching.
    pub fn render_file<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
        options: &FileRender,
    ) -> Result<String, TemplateError> {
        let mut env = self.env.clone();
        env.set_trim_blocks(options.trim_blocks);
        env.set_lstrip_blocks(options.lstrip_blocks);
        let (ctx, _tainted) = context_of(vars.into());
        let mut rendered = env.render_str(text, ctx).map_err(convert_error)?;
        // Measured against the reference: a role's `sshd_config_snippet.j2` reduces to a false
        // `{% if %}` followed by the file's own final newline, and the reference writes that one
        // byte, `\n`. `trim_blocks`
        // strips the newline right after the block tag it closes, even when that newline is also
        // the template's very last character, so `set_keep_trailing_newline(true)` (which only
        // restores a newline the *source* stripping would have dropped, not one `trim_blocks`
        // already consumed) cannot put it back either; measured, dropping the call changes no
        // test here. The source's own final newline is restored directly instead, which covers
        // both cases and matches the reference regardless of what `trim_blocks` did to it.
        if text.ends_with('\n') && !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        Ok(if options.newline_sequence == "\n" {
            rendered
        } else {
            rendered.replace('\n', &options.newline_sequence)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Map, Value};

    use super::*;

    fn templar() -> Templar {
        Templar::new(std::env::temp_dir())
    }

    /// A managed host's value lands in the file as text and is never rendered again.
    ///
    /// The template's own text is the author's; what it reads from a host is data. What would
    /// make this red: `render_file` built on `render_in`, whose extra passes render a result that
    /// still carries a marker - the host's `{{ lookup('pipe', 'touch ...') }}` would run here, on
    /// the controller, while writing the file.
    #[test]
    fn a_host_value_in_a_template_is_written_as_text() {
        let marker =
            std::env::temp_dir().join(format!("volant-render-file-{}", std::process::id()));
        let payload = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());
        let mut map = Map::new();
        map.insert("motd".into(), Value::String(payload.clone()));
        let untrusted: BTreeSet<String> = ["motd".to_string()].into();
        let vars = Vars {
            map: &map,
            hostvars: None,
            shared: None,
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

    /// The final newline of the file survives, and a template that renders to nothing but it
    /// writes it: measured, `sshd_config_snippet.j2` with its `if` false is one byte, `\n`.
    #[test]
    fn the_final_newline_is_kept() {
        assert_eq!(
            templar()
                .render_file(
                    "{% if false %}x{% endif %}\n",
                    &Map::new(),
                    &FileRender::default()
                )
                .unwrap(),
            "\n"
        );
        assert_eq!(
            templar()
                .render_file("a", &Map::new(), &FileRender::default())
                .unwrap(),
            "a"
        );
    }
}
