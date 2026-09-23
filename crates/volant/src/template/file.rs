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
    ///
    /// Two things ansible-core's own `template` action does after Jinja2 renders are reproduced
    /// here rather than left to minijinja, read from the reference's source
    /// (`plugins/action/template.py`, `_post_render_mutation` in
    /// `_internal/_templating/_engine.py`):
    ///
    /// 1. Jinja2's lexer normalises every `\r\n` and lone `\r` in the source to `\n` before it
    ///    ever sees the template; minijinja does not, so the same normalisation happens here.
    /// 2. Jinja2 itself renders with `keep_trailing_newline=False`, which can drop more than the
    ///    reference keeps once a block tag's `trim_blocks` also eats a newline right beside it;
    ///    the reference's own fix is not to change that, but to count the trailing `\n` of the
    ///    original text and of Jinja's result afterwards, and append the difference. Doing the
    ///    same here, on the *original*, non-normalised text, is what makes `trailing_newlines`
    ///    read `text` rather than the normalised copy that was actually rendered.
    pub fn render_file<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
        options: &FileRender,
    ) -> Result<String, TemplateError> {
        let mut env = self.env.clone();
        env.set_trim_blocks(options.trim_blocks);
        env.set_lstrip_blocks(options.lstrip_blocks);
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let (ctx, _tainted) = context_of(vars.into());
        let mut rendered = env.render_str(&normalized, ctx).map_err(convert_error)?;
        let wanted = trailing_newlines(text);
        let got = trailing_newlines(&rendered);
        if wanted > got {
            rendered.extend(std::iter::repeat_n('\n', wanted - got));
        }
        Ok(if options.newline_sequence == "\n" {
            rendered
        } else {
            rendered.replace('\n', &options.newline_sequence)
        })
    }
}

/// The count of literal `\n` characters at the very end of `s`, stopping at the first character
/// that is not one (a `\r` included): on `"x\r\n\r\n"` this is 1, not 2, which is what makes it
/// match the reference's own count on the text it was given, before any `\r\n` normalisation.
fn trailing_newlines(s: &str) -> usize {
    s.chars().rev().take_while(|&c| c == '\n').count()
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
    /// writes it: measured, a role's `sshd_config_snippet.j2` with its `if` false is one byte,
    /// `\n`. The engine drops more than the source's own trailing newlines once a block tag's
    /// `trim_blocks` eats one beside the render's own stripping; each pair below has the same
    /// number of trailing newlines on both sides, however many the source held.
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

    /// Jinja2's lexer rewrites every `\r\n` and lone `\r` in the source to `\n` before it ever
    /// sees the template; minijinja does not. Without the normalisation, `\r` survives into a
    /// render whose `newline_sequence` is still `\n`, and the crlf case below would double up.
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
}
