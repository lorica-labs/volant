// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};

use rstest::rstest;

use super::load_callbacks;
use crate::cli::plays::Plays;
use crate::config::Config;
use crate::render::Renderer;

struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `-vvvv` names the callback in use, and `-vv` the built-in stdout callbacks set aside.
#[rstest]
#[case(None, "default", &["minimal", "oneline"])]
#[case(Some("minimal"), "minimal", &["default", "oneline"])]
#[case(Some("ansible.builtin.oneline"), "oneline", &["default", "minimal"])]
fn the_loader_lines_name_the_stdout_callback(
    #[case] configured: Option<&str>,
    #[case] loaded: &str,
    #[case] skipped: &[&str],
) {
    let mut config = Config::default();
    config.callbacks.stdout = configured.map(str::to_string);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut out = Renderer::with_writer(Box::new(Shared(buf.clone())), false, 79, 4);
    assert_eq!(load_callbacks(&config, &Plays::Files, &mut out), Ok(()));
    let mut want = format!(
        "Loading callback plugin {loaded} of type stdout, v2.0 from the built-in callback\n"
    );
    for name in skipped {
        let _ = writeln!(
            want,
            "Skipping callback '{name}', as we already have a stdout callback."
        );
    }
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        want
    );
}

#[test]
fn a_stdout_callback_this_engine_does_not_have_ends_the_run_with_status_1() {
    let mut config = Config::default();
    config.callbacks.stdout = Some("community.general.unixy".into());
    let mut out = Renderer::with_writer(Box::new(std::io::sink()), false, 79, 0);
    assert_eq!(load_callbacks(&config, &Plays::Files, &mut out), Err(1));
}
