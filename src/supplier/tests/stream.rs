use super::*;

/// The terminal record is extended ADDITIVELY: every field an old-shape
/// reader knows is exactly what it was, and the two new ones are absent
/// unless there is something to say.
#[test]
fn the_terminal_record_carries_the_outcome_without_disturbing_an_old_reader() {
    let plain = GyldOutputRecord::end("run-1", 4, &Some("gianni".into()), 0);
    let accepted = plain.clone().leaving(Some("/decisions/a.gyld.py".into()));
    let refused = plain.clone().refusing(Some(Refusal {
        stream: "stream-a".into(),
        code: "SELECTION_NOT_OFFERED".into(),
        message: "VersionPin does not offer SdaxRs".into(),
        details: Some(serde_json::json!({ "ruling": "R" })),
        restored: true,
    }));

    // An old-shape reader: the record's fields as they were, on all three.
    for record in [&plain, &accepted, &refused] {
        let seen: serde_json::Value = serde_json::from_slice(&record.to_bytes()).unwrap();
        assert_eq!(seen["run_id"], "run-1");
        assert_eq!(seen["seq"], 4);
        assert_eq!(seen["principal"], "gianni");
        assert_eq!(seen["stream"], "end");
        assert_eq!(seen["done"], true);
        assert_eq!(seen["exit"], 0);
    }
    let bare: serde_json::Value = serde_json::from_slice(&plain.to_bytes()).unwrap();
    assert!(bare.get("refusal").is_none() && bare.get("overlay_file").is_none());

    let seen: serde_json::Value = serde_json::from_slice(&accepted.to_bytes()).unwrap();
    assert_eq!(seen["overlay_file"], "/decisions/a.gyld.py");
    assert!(seen.get("refusal").is_none());

    let seen: serde_json::Value = serde_json::from_slice(&refused.to_bytes()).unwrap();
    assert_eq!(seen["refusal"]["code"], "SELECTION_NOT_OFFERED");
    assert_eq!(seen["refusal"]["stream"], "stream-a");
    assert_eq!(seen["refusal"]["restored"], true);
    assert_eq!(seen["refusal"]["details"]["ruling"], "R");
    assert!(seen.get("overlay_file").is_none());
}
