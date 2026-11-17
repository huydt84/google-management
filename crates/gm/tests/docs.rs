//! Docs follow the code: every route, `GM_*` variable, topic, table and CLI command the code defines
//! is documented in `docs/spec/`, every relative link in the docs resolves, and docs carry no
//! decision IDs that only make sense next to a conversation. No database needed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regex::Regex;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo() -> PathBuf {
    crate_dir().join("../..")
}

fn read(p: impl AsRef<Path>) -> String {
    let p = p.as_ref();
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn spec(name: &str) -> String {
    read(repo().join("docs/spec").join(name))
}

/// Items the code defines that the doc doesn't mention.
fn missing(items: BTreeSet<String>, doc: &str, render: impl Fn(&str) -> String) -> Vec<String> {
    items
        .into_iter()
        .filter(|i| !doc.contains(&render(i)))
        .collect()
}

#[test]
fn every_route_is_in_the_interfaces_spec() {
    let re = Regex::new(r#"\.route\(\s*"([^"]+)""#).unwrap();
    let routes: BTreeSet<String> = re
        .captures_iter(&read(crate_dir().join("src/web/mod.rs")))
        .map(|c| c[1].to_string())
        .collect();
    assert!(routes.len() > 10, "route scan found {routes:?}");
    let gaps = missing(routes, &spec("interfaces.md"), |r| format!("`{r}"));
    assert!(
        gaps.is_empty(),
        "routes missing from docs/spec/interfaces.md: {gaps:?}"
    );
}

#[test]
fn every_setting_is_in_the_interfaces_spec() {
    let src = read(crate_dir().join("src/config.rs"));
    let call = Regex::new(r#"\b(?:env|env_or|parse|required)\(\s*"([A-Z][A-Z0-9_]+)""#).unwrap();
    let literal = Regex::new(r#""GM_([A-Z][A-Z0-9_]+)""#).unwrap();
    let vars: BTreeSet<String> = call
        .captures_iter(&src)
        .chain(literal.captures_iter(&src))
        .map(|c| format!("GM_{}", &c[1]))
        .collect();
    assert!(vars.len() > 20, "setting scan found {vars:?}");
    let gaps = missing(vars, &spec("interfaces.md"), |v| format!("`{v}`"));
    assert!(
        gaps.is_empty(),
        "settings missing from docs/spec/interfaces.md: {gaps:?}"
    );
}

#[test]
fn every_topic_is_in_the_events_spec() {
    let lib = read(crate_dir().join("src/lib.rs"));
    let module = &lib[lib.find("pub mod topics").expect("topics module")..];
    let re = Regex::new(r#"pub const \w+: &str = "([a-z.]+)""#).unwrap();
    let topics: BTreeSet<String> = re.captures_iter(module).map(|c| c[1].to_string()).collect();
    assert!(topics.len() > 5, "topic scan found {topics:?}");
    let gaps = missing(topics, &spec("events.md"), |t| format!("`{t}`"));
    assert!(
        gaps.is_empty(),
        "topics missing from docs/spec/events.md: {gaps:?}"
    );
}

#[test]
fn every_table_is_in_the_data_model_spec() {
    let re = Regex::new(r"CREATE TABLE (\w+)").unwrap();
    let mut tables = BTreeSet::new();
    for entry in std::fs::read_dir(crate_dir().join("migrations")).unwrap() {
        for c in re.captures_iter(&read(entry.unwrap().path())) {
            tables.insert(c[1].to_string());
        }
    }
    assert!(tables.len() > 5, "table scan found {tables:?}");
    let gaps = missing(tables, &spec("data-model.md"), |t| format!("`{t}`"));
    assert!(
        gaps.is_empty(),
        "tables missing from docs/spec/data-model.md: {gaps:?}"
    );
}

#[test]
fn every_cli_command_is_in_the_interfaces_spec() {
    let main = read(crate_dir().join("src/main.rs"));
    let body = &main[main.find("enum Cmd").expect("Cmd enum")..];
    let body = &body[..body.find("\n}").unwrap()];
    let variant = Regex::new(r"(?m)^    ([A-Z]\w*)\b").unwrap();
    let kebab = Regex::new(r"([a-z0-9])([A-Z])").unwrap();
    let commands: BTreeSet<String> = variant
        .captures_iter(body)
        .map(|c| kebab.replace_all(&c[1], "$1-$2").to_lowercase())
        .collect();
    assert!(commands.len() >= 5, "command scan found {commands:?}");
    let gaps = missing(commands, &spec("interfaces.md"), |c| format!("`gm {c}"));
    assert!(
        gaps.is_empty(),
        "commands missing from docs/spec/interfaces.md: {gaps:?}"
    );
}

fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(p);
            }
        }
    }
    let root = repo();
    let mut files = vec![
        root.join("README.md"),
        root.join("CLAUDE.md"),
        root.join("deploy/README.md"),
    ];
    walk(&root.join("docs"), &mut files);
    files
}

#[test]
fn every_relative_doc_link_resolves() {
    let link = Regex::new(r"\]\(([^)\s#]+)(?:#[^)]*)?\)").unwrap();
    let mut broken = vec![];
    for f in markdown_files() {
        for c in link.captures_iter(&read(&f)) {
            let target = &c[1];
            if target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            if !f.parent().unwrap().join(target).exists() {
                broken.push(format!("{} -> {target}", f.display()));
            }
        }
    }
    assert!(broken.is_empty(), "broken doc links: {broken:#?}");
}

#[test]
fn docs_carry_no_decision_ids() {
    // "decision P4", "(N20)", "L12" read as noise to anyone who wasn't in the conversation. Threat
    // IDs (T1…) are defined in, and only used within, the security spec.
    let id = Regex::new(
        r"\bdecisions? [A-Z]\d{1,2}\b|\([A-Z]\d{1,2}(?:, ?[A-Z]\d{1,2})*\)|decisions\.md",
    )
    .unwrap();
    let mut found = vec![];
    for f in markdown_files() {
        for (n, line) in read(&f).lines().enumerate() {
            if id.is_match(line) {
                found.push(format!("{}:{}: {line}", f.display(), n + 1));
            }
        }
    }
    assert!(found.is_empty(), "decision IDs in docs: {found:#?}");
}
