use pithos::config::{PostgresField, env_config, load};

const POSTGRES: &str = "postgres: {version: \"18.6\", database: app}\n";

fn values(field: PostgresField) -> String {
    match field {
        PostgresField::Host => "pithos-postgres".into(),
        PostgresField::Port => "5432".into(),
        PostgresField::User => "postgres".into(),
        PostgresField::Password => "pw".into(),
        PostgresField::Database => "app".into(),
        PostgresField::Url => "postgresql://postgres:pw@pithos-postgres:5432/app".into(),
    }
}

fn rendered(text: &str) -> String {
    let config = load(text.as_bytes()).unwrap();
    env_config(&config).unwrap().unwrap().render(&values)
}

#[test]
fn absent_env_means_nothing_extra() {
    let config = load(b"toolchains: {}\n").unwrap();
    assert!(env_config(&config).unwrap().is_none());
}

#[test]
fn env_renders_literal_values_in_declared_order() {
    assert_eq!(
        rendered(
            "toolchains: {}\nenv:\n  ZED: \"1\"\n  ConnectionStrings__app-admin: \"Host=db;Password=a$b\"\n  A.B: x\n"
        ),
        "ZED=1\nConnectionStrings__app-admin=Host=db;Password=a$b\nA.B=x\n"
    );
}

#[test]
fn env_fills_postgres_placeholders_and_keeps_an_escaped_dollar() {
    let text = format!(
        "toolchains: {{}}\n{POSTGRES}env:\n  \
         APP_DB_URL: \"${{postgres.url}}\"\n  \
         ADMIN: \"Host=${{postgres.host}};Port=${{postgres.port}};Database=${{postgres.database}};Username=${{postgres.user}};Password=${{postgres.password}}\"\n  \
         LITERAL: \"$${{postgres.url}} costs $$5\"\n"
    );
    assert_eq!(
        rendered(&text),
        "APP_DB_URL=postgresql://postgres:pw@pithos-postgres:5432/app\n\
         ADMIN=Host=pithos-postgres;Port=5432;Database=app;Username=postgres;Password=pw\n\
         LITERAL=${postgres.url} costs $5\n"
    );
}

#[test]
fn env_knows_whether_it_needs_the_database() {
    let config =
        load(format!("toolchains: {{}}\n{POSTGRES}env: {{A: \"${{postgres.url}}\"}}\n").as_bytes())
            .unwrap();
    assert!(env_config(&config).unwrap().unwrap().uses_postgres());
    let config = load(b"toolchains: {}\nenv: {A: \"$${postgres.url}\"}\n").unwrap();
    assert!(!env_config(&config).unwrap().unwrap().uses_postgres());
}

#[test]
fn malformed_env_is_rejected_with_an_env_error() {
    for (env, reason) in [
        ("null", "must be a mapping"),
        ("[A]", "must be a mapping"),
        ("{1A: x}", "is not a valid variable name"),
        ("{\"A B\": x}", "is not a valid variable name"),
        ("{\"A=B\": x}", "is not a valid variable name"),
        ("{\"\": x}", "is not a valid variable name"),
        ("{PITHOS_POSTGRES_URL: x}", "is reserved for Pithos"),
        ("{pithos_anything: x}", "is reserved for Pithos"),
        ("{GIT_CONFIG_COUNT: \"2\"}", "is reserved for Pithos"),
        ("{A: 5}", "must be a quoted string"),
        ("{A: true}", "must be a quoted string"),
        ("{A: [x]}", "must be a quoted string"),
        ("{A: \"x\\ny\"}", "must be a single line"),
        ("{A: \"x\\ry\"}", "must be a single line"),
        ("{A: \"x\\0y\"}", "must be a single line"),
        (
            "{A: \"${postgres.secret}\"}",
            "unknown placeholder `${postgres.secret}`",
        ),
        ("{A: \"${HOME}\"}", "unknown placeholder `${HOME}`"),
        ("{A: \"${postgres.url\"}", "unclosed placeholder"),
        // Without a postgres block there is no database to point at.
        ("{A: \"${postgres.url}\"}", "needs a `postgres` block"),
    ] {
        let text = format!("toolchains: {{}}\nenv: {env}\n");
        let error = load(text.as_bytes()).unwrap_err().to_string();
        assert!(error.starts_with(".pithos env: "), "{env}: {error}");
        assert!(error.contains(reason), "{env}: {error}");
    }
}

#[test]
fn env_errors_never_echo_a_value() {
    let error = load(b"toolchains: {}\nenv: {A: \"secret-canary\\n${nope}\"}\n")
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret-canary"), "{error}");
}
