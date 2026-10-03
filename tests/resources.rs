//! The resource catalogue, tested directly against its public functions.
//!
//! Relocated here from `src/resources.rs`. These use SQLite in memory where a real database is
//! needed, because `Store` is the only thing that reads history; nothing here needs a container.
//! Protocol-level reachability lives in `tests/mcp_features.rs`.

use poc_rust_mcp::{
    db::Store,
    resources::{self, HISTORY_URI, NO_HISTORY, OPERATION_URI_PREFIX, Target},
};
use rmcp::model::{ReadResourceResult, ResourceContents};

#[test]
fn the_listed_resource_is_the_one_the_prompts_name() {
    let catalogue = resources::list();
    let listed: Vec<&str> = catalogue
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect();
    assert_eq!(listed, [HISTORY_URI]);
    // `prompts` points clients at this URI by constant, so a rename cannot drift unnoticed.
    assert_eq!(listed, [crate::resources::HISTORY_URI]);
}

#[test]
fn every_listed_uri_and_template_parses() {
    for resource in resources::list() {
        assert!(
            resources::parse(&resource.uri).is_ok(),
            "listed resource {} does not parse",
            resource.uri
        );
    }
    for uri in [
        "calc://history/1",
        "calc://history/4242",
        "calc://history/operation/add",
        "calc://history/operation/div",
    ] {
        assert!(resources::parse(uri).is_ok(), "{uri} should parse");
    }
}

#[test]
fn parsing_picks_the_right_target() {
    assert_eq!(
        resources::parse(HISTORY_URI).expect("valid"),
        Target::History
    );
    assert_eq!(
        resources::parse("calc://history/7").expect("valid"),
        Target::Entry(7)
    );
    assert_eq!(
        resources::parse("calc://history/operation/mul").expect("valid"),
        Target::Operation("mul".to_owned())
    );
}

#[test]
fn a_uri_this_server_does_not_serve_is_refused_by_name() {
    for uri in [
        "file:///etc/passwd",
        "calc://",
        "calc://other",
        "https://example.com/history",
    ] {
        let error = resources::parse(uri).expect_err("must be refused");
        assert!(error.message.contains(uri), "{error}");
    }
}

#[test]
fn a_bogus_id_or_operation_is_refused_rather_than_left_to_404_later() {
    // These would otherwise resolve to a resource that can never have content, which reads to
    // a client as a broken server rather than a bad request.
    for uri in [
        "calc://history/latest",
        "calc://history/0",
        "calc://history/-1",
        "calc://history/1/2",
        "calc://history/operation/banana",
        "calc://history/operation/",
    ] {
        assert!(resources::parse(uri).is_err(), "{uri} should be refused");
    }
}

#[test]
fn a_write_invalidates_the_history_resources_but_never_a_row() {
    let invalidated = resources::invalidated_by_write("add");
    assert!(invalidated.contains(&HISTORY_URI.to_owned()));
    assert!(invalidated.contains(&format!("{OPERATION_URI_PREFIX}add")));
    assert!(
        !invalidated.iter().any(|uri| uri.contains("{id}")),
        "a single row never changes once written: {invalidated:?}"
    );
}

#[tokio::test]
async fn a_disabled_store_still_resolves_the_resource_with_an_explanation() {
    // Storage degrades rather than fails, and a disabled database is not a missing resource.
    let result = resources::read(&Store::disabled(), HISTORY_URI)
        .await
        .expect("a disabled database must not fail the read");
    let ResourceContents::TextResourceContents { text, .. } = &result.contents[0] else {
        panic!("expected text contents: {:?}", result.contents);
    };
    assert!(
        text.contains("DATABASE_URL"),
        "the content should explain what to set, got {text}"
    );
}

#[tokio::test]
async fn an_empty_database_reports_no_history_rather_than_an_error() {
    let store = Store::connect("sqlite::memory:");
    let body = |result: ReadResourceResult| match &result.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => panic!("expected text contents, got {other:?}"),
    };

    assert_eq!(
        body(
            resources::read(&store, HISTORY_URI)
                .await
                .expect("an empty database resolves")
        ),
        NO_HISTORY
    );
    assert_eq!(
        body(
            resources::read(&store, "calc://history/operation/add")
                .await
                .expect("an empty database resolves")
        ),
        "no `add` calculations recorded"
    );
}

#[tokio::test]
async fn an_id_with_no_row_is_resource_not_found() {
    // A real, migrated, empty database: the read succeeds, there is simply no such id.
    let store = Store::connect("sqlite::memory:");
    let error = resources::read(&store, "calc://history/999")
        .await
        .expect_err("must not resolve");
    assert_eq!(error.code.0, -32002, "expected RESOURCE_NOT_FOUND: {error}");
    assert!(error.message.contains("999"), "{error}");
}

#[tokio::test]
async fn completion_of_a_non_placeholder_is_an_error() {
    let error = resources::complete(&Store::disabled(), HISTORY_URI, "id", "")
        .await
        .expect_err("must be rejected");
    assert!(error.message.contains("placeholder"), "{error}");
}
