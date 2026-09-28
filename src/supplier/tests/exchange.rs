use super::*;

#[test]
fn a_document_over_the_budget_is_refused_not_returned() {
    let dir = std::env::temp_dir().join(format!("glade-gyld-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streams.json");
    std::fs::write(&path, "x".repeat(100)).unwrap();

    assert_eq!(read_bounded(&path, 1000).unwrap().len(), 100);
    let e = read_bounded(&path, 10).unwrap_err();
    assert!(e.contains("the exchange budget is 10"), "{e}");
    let e = read_bounded(&dir.join("missing.json"), 10).unwrap_err();
    assert!(e.contains("cannot read"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
