use super::*;

#[test]
fn test_instance_current_image_wrapper_matches_the_actual_process() {
    let image = file::image(&std::env::current_exe().unwrap()).unwrap();
    assert_eq!(verify_current_image(&image), Ok(()));
}

#[test]
fn test_instance_current_image_wrapper_refuses_an_unmapped_file_token() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unmapped");
    std::fs::write(&path, b"unmapped image sentinel").unwrap();
    let image = file::image(&path).unwrap();
    assert_eq!(verify_current_image(&image), Err(Unverified::Identity));
}
