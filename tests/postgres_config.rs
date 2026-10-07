use pithos::config::{PostgresConfig, load, postgres_config};

#[test]
fn absent_postgres_means_no_database() {
    let config = load(b"toolchains: {}\n").unwrap();
    assert_eq!(postgres_config(&config).unwrap(), None);
}

#[test]
fn postgres_needs_an_exact_version_and_a_database_name() {
    let text = "toolchains: {}\npostgres: {version: \"17.10\", database: budgetoid}\n";
    let config = load(text.as_bytes()).unwrap();
    assert_eq!(
        postgres_config(&config).unwrap(),
        Some(PostgresConfig {
            version: "17.10".into(),
            database: "budgetoid".into(),
            max_connections: None,
        })
    );
    // Underscores and a 63-byte name (Postgres' identifier limit) are fine.
    let long = format!("_{}", "a".repeat(62));
    let text = format!("toolchains: {{}}\npostgres: {{version: \"18.3\", database: {long}}}\n");
    assert_eq!(
        postgres_config(&load(text.as_bytes()).unwrap())
            .unwrap()
            .unwrap()
            .database,
        long
    );
}

#[test]
fn malformed_postgres_is_rejected_with_a_postgres_error() {
    for (postgres, reason) in [
        ("null", "must be a mapping"),
        ("{}", "missing required key `version`"),
        ("{version: \"17.10\"}", "missing required key `database`"),
        // A major alone is a floating tag; unquoted 17.10 is a YAML float.
        (
            "{version: \"17\", database: app}",
            "version must be an exact `major.minor`",
        ),
        (
            "{version: \"17.10.1\", database: app}",
            "version must be an exact `major.minor`",
        ),
        (
            "{version: latest, database: app}",
            "version must be an exact `major.minor`",
        ),
        (
            "{version: 17.10, database: app}",
            "version must be a quoted string",
        ),
        (
            "{version: \"17.10\", database: Budget}",
            "database must match",
        ),
        ("{version: \"17.10\", database: 1db}", "database must match"),
        ("{version: \"17.10\", database: a-b}", "database must match"),
        (
            "{version: \"17.10\", database: \"\"}",
            "database must match",
        ),
        (
            "{version: \"17.10\", database: app, image: evil}",
            "unknown key `image`",
        ),
        (
            "{version: \"17.10\", database: app, max_connections: 19}",
            "max_connections must be an integer from 20 to 1000",
        ),
        (
            "{version: \"17.10\", database: app, max_connections: 1001}",
            "max_connections must be an integer from 20 to 1000",
        ),
        (
            "{version: \"17.10\", database: app, max_connections: \"500\"}",
            "max_connections must be an integer from 20 to 1000",
        ),
        (
            "{version: \"17.10\", database: app, max_connections: 50.5}",
            "max_connections must be an integer from 20 to 1000",
        ),
    ] {
        let text = format!("toolchains: {{}}\npostgres: {postgres}\n");
        let error = load(text.as_bytes()).unwrap_err().to_string();
        assert!(
            error.starts_with(".pithos postgres: "),
            "{postgres}: {error}"
        );
        assert!(error.contains(reason), "{postgres}: {error}");
    }
    let too_long = format!("a{}", "b".repeat(63));
    let text =
        format!("toolchains: {{}}\npostgres: {{version: \"17.10\", database: {too_long}}}\n");
    assert!(load(text.as_bytes()).is_err());
}

#[test]
fn postgres_may_raise_its_connection_limit() {
    // Test suites that open a database per test outgrow Postgres' default 100.
    for (value, expected) in [("20", 20), ("500", 500), ("1000", 1000)] {
        let text = format!(
            "toolchains: {{}}\npostgres: {{version: \"18.6\", database: app, max_connections: {value}}}\n"
        );
        assert_eq!(
            postgres_config(&load(text.as_bytes()).unwrap())
                .unwrap()
                .unwrap()
                .max_connections,
            Some(expected)
        );
    }
}
