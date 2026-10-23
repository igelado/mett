use super::*;

fn folder(id: &str, parent: &str, name: &str, element: &str, class: Option<&str>) -> EwsFolder {
    EwsFolder {
        id: id.into(),
        parent: Some(parent.into()),
        name: name.into(),
        element: element.into(),
        class: class.map(str::to_owned),
        unread: 0,
        total: 0,
    }
}

#[test]
fn folders_come_in_tree_order_by_name() {
    let folders = vec![
        folder("b", "root", "beta", "Folder", Some("IPF.Note")),
        folder("a2", "a", "Two", "Folder", None),
        folder("a", "root", "Alpha", "Folder", Some("IPF.Note")),
        folder("a1", "a", "one", "Folder", Some("IPF.Note")),
    ];
    let ids: Vec<String> = tree_order(folders, Some("root"))
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids, ["a", "a1", "a2", "b"]);
}

#[test]
fn only_mail_folders_are_listed() {
    let folders = vec![
        folder("inbox", "root", "Inbox", "Folder", Some("IPF.Note")),
        folder(
            "cal",
            "root",
            "Calendar",
            "CalendarFolder",
            Some("IPF.Appointment"),
        ),
        folder("below-cal", "cal", "Mail?", "Folder", Some("IPF.Note")),
        folder("search", "root", "Finder", "SearchFolder", Some("IPF.Note")),
        folder("tasks", "root", "Tasks", "Folder", Some("IPF.Task")),
        folder("plain", "inbox", "Plain", "Folder", None),
    ];
    let ids: Vec<String> = mail_folders(folders).into_iter().map(|f| f.id).collect();
    assert_eq!(ids, ["inbox", "plain"]);
}

#[test]
fn large_requests_and_downloads_get_more_time() {
    assert_eq!(timeout_for(100, Duration::ZERO), REQUEST_TIMEOUT);
    assert_eq!(
        timeout_for(10 * BYTES_PER_SECOND, Duration::ZERO),
        REQUEST_TIMEOUT + Duration::from_secs(10)
    );
    assert_eq!(timeout_for(100, DOWNLOAD_TIMEOUT), DOWNLOAD_TIMEOUT);
}

#[test]
fn decodes_base64_wrapped_in_white_space() {
    use base64::Engine;

    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    let wrapped: String = encoded
        .as_bytes()
        .chunks(76)
        .flat_map(|line| [std::str::from_utf8(line).unwrap(), "\r\n"])
        .collect();
    assert_eq!(decode_base64(&wrapped).unwrap(), data);
    assert_eq!(decode_base64("").unwrap(), b"");
    assert_eq!(decode_base64(" TW\nE= ").unwrap(), b"Ma");
    assert!(decode_base64("TW=E").is_err());
    // Padding ends the data, wherever the blocks fall.
    let padded = format!("{}AA==", "AAAA".repeat(1023));
    assert_eq!(decode_base64(&padded).unwrap().len(), 3070);
    assert!(decode_base64(&format!("{padded}AAAA")).is_err());
    assert!(decode_base64("TWFu\u{e9}").is_err());
}

#[test]
fn folders_sort_by_name_without_regard_to_case() {
    let folder = |name: &str, id: &str| EwsFolder {
        name: name.into(),
        id: id.into(),
        parent: None,
        element: "Folder".into(),
        class: None,
        unread: 0,
        total: 0,
    };
    let mut folders = vec![folder("b", "2"), folder("B", "1"), folder("A", "3")];
    sort_by_name(&mut folders);
    let ids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["3", "1", "2"]);
}

#[test]
fn cursors_belong_to_their_listing() {
    let folder = FolderId::new("AQMk/a+b=");
    let c = cursor(50, "all", &folder);
    assert_eq!(parse_cursor(&c, &folder, "all").unwrap(), 50);
    assert!(parse_cursor(&c, &FolderId::new("other"), "all").is_err());
    assert!(parse_cursor(&c, &folder, "2026-01-01T00:00:00Z").is_err());
}

#[test]
fn messages_follow_the_schema_order() {
    let draft = Draft {
        from: Some(Address::new("me@contoso.com")),
        to: vec![Address::new("a@x.org").with_name("A & B")],
        bcc: vec![Address::new("b@x.org")],
        subject: "<hi>".into(),
        body: "text".into(),
        ..Draft::default()
    };
    let xml = message_xml(&draft);
    let order: Vec<usize> = [
        "<t:Subject>",
        "<t:Body",
        "<t:ToRecipients>",
        "<t:BccRecipients>",
        "<t:From>",
    ]
    .iter()
    .map(|tag| xml.find(tag).unwrap())
    .collect();
    assert!(order.is_sorted(), "{xml}");
    assert!(xml.contains("<t:Subject>&lt;hi&gt;</t:Subject>"));
    assert!(xml.contains("<t:Name>A &amp; B</t:Name>"));
    assert!(!xml.contains("CcRecipients"));
}
