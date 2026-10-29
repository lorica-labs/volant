// SPDX-License-Identifier: GPL-3.0-or-later
//! The report as ansible-core's `_junit_xml` builds it and minidom's `toprettyxml` writes it.

use std::fmt::Write as _;

use super::decimal::Dec;

pub(super) struct Case {
    pub(super) name: String,
    pub(super) classname: String,
    pub(super) time: Dec,
    pub(super) body: Body,
}

pub(super) enum Body {
    Out(String),
    Skipped(String),
    Error { message: String, output: String },
    Failure { message: String, output: String },
}

/// How minidom escapes, which changed with Python 3.13: before it, text and attribute values
/// alike lose `&`, `<`, `"` and `>`; from it, text keeps its quotes and an attribute value also
/// loses its line breaks and tabs.
#[derive(Clone, Copy)]
pub(super) struct Escape {
    /// The controller's Python is older than 3.13.
    pub(super) legacy: bool,
}

impl Escape {
    fn data(self, text: &str) -> String {
        let text = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if self.legacy {
            text.replace('"', "&quot;")
        } else {
            text
        }
    }

    fn attr(self, text: &str) -> String {
        let text = self.data(text);
        if self.legacy {
            return text;
        }
        text.replace('"', "&quot;")
            .replace('\r', "&#13;")
            .replace('\n', "&#10;")
            .replace('\t', "&#9;")
    }
}

/// `TestSuites(suites=[TestSuite(name, cases)]).to_pretty_xml()`, as minidom writes it.
pub(super) fn suites(playbook: &str, cases: &[Case], escape: Escape) -> String {
    let count = |want: fn(&Body) -> bool| cases.iter().filter(|c| want(&c.body)).count();
    let errors = count(|b| matches!(b, Body::Error { .. }));
    let failures = count(|b| matches!(b, Body::Failure { .. }));
    let skipped = count(|b| matches!(b, Body::Skipped(_)));
    let mut total = Dec::zero();
    for case in cases {
        if !case.time.is_zero() {
            total = total.add(&case.time);
        }
    }
    // `decimal.Decimal(sum(...))` of the suites' times, one suite here.
    let all = Dec::zero().add(&total);
    let mut out = String::from("<?xml version=\"1.0\" ?>\n");
    let _ = writeln!(
        out,
        "<testsuites disabled=\"0\" errors=\"{errors}\" failures=\"{failures}\" tests=\"{}\" time=\"{}\">",
        cases.len(),
        all.text()
    );
    let open = format!(
        "\t<testsuite disabled=\"0\" errors=\"{errors}\" failures=\"{failures}\" name=\"{}\" skipped=\"{skipped}\" tests=\"{}\" time=\"{}\"",
        escape.attr(playbook),
        cases.len(),
        total.text()
    );
    if cases.is_empty() {
        let _ = writeln!(out, "{open}/>");
    } else {
        let _ = writeln!(out, "{open}>");
        for case in cases {
            case_xml(&mut out, case, escape);
        }
        out.push_str("\t</testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

fn case_xml(out: &mut String, case: &Case, escape: Escape) {
    let open = format!(
        "\t\t<testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
        escape.attr(&case.classname),
        escape.attr(&case.name),
        case.time.text()
    );
    let child = match &case.body {
        Body::Out(text) if text.is_empty() => None,
        Body::Out(text) => Some(format!("<system-out>{}</system-out>", escape.data(text))),
        Body::Skipped(text) if text.is_empty() => Some("<skipped/>".to_string()),
        Body::Skipped(text) => Some(format!("<skipped>{}</skipped>", escape.data(text))),
        Body::Error { message, output } => Some(element("error", message, output, escape)),
        Body::Failure { message, output } => Some(element("failure", message, output, escape)),
    };
    match child {
        None => {
            let _ = writeln!(out, "{open}/>");
        }
        Some(child) => {
            let _ = writeln!(out, "{open}>\n\t\t\t{child}\n\t\t</testcase>");
        }
    }
}

fn element(tag: &str, message: &str, output: &str, escape: Escape) -> String {
    let attrs = format!("message=\"{}\" type=\"{tag}\"", escape.attr(message));
    if output.is_empty() {
        format!("<{tag} {attrs}/>")
    } else {
        format!("<{tag} {attrs}>{}</{tag}>", escape.data(output))
    }
}

#[cfg(test)]
mod tests {
    use super::super::decimal::exact;
    use super::*;

    fn one() -> Vec<Case> {
        vec![Case {
            name: "[h1] all: first _raw_params=echo hi".into(),
            classname: "/p/play.yml:4".into(),
            time: exact(0.5),
            body: Body::Out("{\n\"changed\": true\n}".into()),
        }]
    }

    #[test]
    fn the_xml_is_minidom_s() {
        assert_eq!(
            suites("play", &one(), Escape { legacy: false }),
            "<?xml version=\"1.0\" ?>\n\
             <testsuites disabled=\"0\" errors=\"0\" failures=\"0\" tests=\"1\" time=\"0.5\">\n\
             \t<testsuite disabled=\"0\" errors=\"0\" failures=\"0\" name=\"play\" skipped=\"0\" tests=\"1\" time=\"0.5\">\n\
             \t\t<testcase classname=\"/p/play.yml:4\" name=\"[h1] all: first _raw_params=echo hi\" time=\"0.5\">\n\
             \t\t\t<system-out>{\n\"changed\": true\n}</system-out>\n\
             \t\t</testcase>\n\
             \t</testsuite>\n\
             </testsuites>\n"
        );
    }

    /// Python before 3.13 escapes the quotes of text too.
    #[test]
    fn an_older_python_escapes_the_quotes_of_text() {
        let xml = suites("play", &one(), Escape { legacy: true });
        assert!(
            xml.contains("<system-out>{\n&quot;changed&quot;: true\n}</system-out>"),
            "{xml}"
        );
    }
}
