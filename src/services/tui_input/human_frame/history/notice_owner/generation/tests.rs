use super::super::context_observation_tests as io;
use super::*;
use crate::services::tui_input::human_frame::history::notice_owner::context_observation;

#[test]
fn context_dependencies_stat_each_unique_path_once_per_work_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context.json");
    std::fs::write(&path, b"context").unwrap();
    for rows in [1, 64, 1024] {
        let mut observations = context_observation::Observations::default();
        io::take(&path);
        let first = context_dependencies(std::iter::repeat_n(&path, rows), &mut observations);
        assert_eq!(first.len(), 1);
        assert_eq!(io::take(&path).stats, 1);
        let second = context_dependencies(std::iter::repeat_n(&path, rows), &mut observations);
        assert_eq!(first, second);
        let weak = usize::from(first[0].1.as_ref().is_some_and(|stamp| stamp.ino == 0));
        assert_eq!(io::take(&path).stats, weak);
    }
}
