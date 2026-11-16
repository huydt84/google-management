use gm::gmail::message::ParsedMessage;
use gm::rules::{Action, Priority, RuleSet};

fn msg(from: &str, name: &str, subject: &str, labels: &[&str]) -> ParsedMessage {
    ParsedMessage {
        from_addr: from.into(),
        from_name: name.into(),
        subject: subject.into(),
        label_ids: labels.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn repository_rules_file_is_valid_and_examples_pass() {
    let path = format!("{}/../../rules.yaml", env!("CARGO_MANIFEST_DIR"));
    let rs = RuleSet::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let errors = rs.check_examples();
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(rs.default, Action::Triage);
}

const YAML: &str = r#"
default: triage
rules:
  - name: card-flash
    when: { from: "info@card.example", subject: "速報" }
    action: notify
    priority: silent
    summarize: false
    extract: { amount: '本人\s*([\d,]+)\s*円' }
    message: "Card ¥{amount} / {missing}"
  - name: shop
    when: { from: "*@shop.example", labels_none: [CATEGORY_PROMOTIONS] }
    action: digest
  - name: promos
    when: { labels_any: [CATEGORY_PROMOTIONS] }
    action: ignore
  - name: named
    when: { from_name: "^News" , list_unsubscribe: true }
    action: digest
    section: newsletters
    summarize: true
"#;

#[test]
fn first_match_extract_and_render() {
    let rs = RuleSet::parse(YAML).unwrap();
    let mut m = msg("INFO@card.example", "", "【速報版】", &[]);
    m.from_addr = m.from_addr.to_lowercase();
    m.body_text = "ご利用日 2026/01/01 本人 12,345 円".into();
    let v = rs.evaluate(&m);
    assert_eq!(v.rule, "card-flash");
    assert_eq!(
        (v.action, v.priority, v.summarize),
        (Action::Notify, Priority::Silent, false)
    );
    assert_eq!(v.extracted["amount"], "12,345");
    assert_eq!(v.message.as_deref(), Some("Card ¥12,345 / ?"));
}

#[test]
fn globs_labels_and_defaults() {
    let rs = RuleSet::parse(YAML).unwrap();
    let v = rs.evaluate(&msg("a@shop.example", "", "x", &["INBOX"]));
    assert_eq!(
        (v.rule.as_str(), v.action, v.section.as_deref(), v.summarize),
        ("shop", Action::Digest, Some("other"), false)
    );
    assert_eq!(
        rs.evaluate(&msg("a@shop.example", "", "x", &["CATEGORY_PROMOTIONS"]))
            .rule,
        "promos"
    );
    assert_eq!(
        rs.evaluate(&msg("a@notshop.example.com", "", "x", &[]))
            .rule,
        "default"
    );
    let d = rs.evaluate(&msg("x@y.z", "", "x", &[]));
    assert_eq!((d.action, d.summarize), (Action::Triage, true));

    let mut n = msg("n@x.y", "News Daily", "x", &[]);
    assert_eq!(
        rs.evaluate(&n).rule,
        "default",
        "list_unsubscribe must match too"
    );
    n.has_list_unsubscribe = true;
    let v = rs.evaluate(&n);
    assert_eq!(
        (v.rule.as_str(), v.section.as_deref(), v.summarize),
        ("named", Some("newsletters"), true)
    );
}

#[test]
fn invalid_files_are_rejected() {
    assert!(RuleSet::parse("rules: [{name: a, when: {subject: '('}, action: ignore}]").is_err());
    assert!(
        RuleSet::parse("rules: [{name: a, when: {}, action: ignore}]").is_err(),
        "no conditions"
    );
    assert!(
        RuleSet::parse("rules: [{name: a, when: {nope: 1}, action: ignore}]").is_err(),
        "unknown key"
    );
    assert!(RuleSet::parse("rules: [{name: a, when: {subject: x}, action: ignore}, {name: a, when: {subject: y}, action: ignore}]").is_err());
}

#[test]
fn examples_that_land_elsewhere_are_reported() {
    let rs = RuleSet::parse(
        "rules:\n  - name: a\n    when: {subject: x}\n    action: ignore\n  - name: b\n    when: {subject: xy}\n    action: ignore\n    examples: [{from: 'q@w.e', subject: xy}]\n",
    )
    .unwrap();
    let errs = rs.check_examples();
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("matched \"a\""));
}
