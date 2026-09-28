use pithos::broker::compose::{ComposeError, parse};

fn rejects(input: &str) {
    assert!(parse(input).is_err(), "policy accepted invalid fixture");
}

#[test]
fn schema_is_an_allowlist_not_a_compose_passthrough() {
    for field in [
        "ports",
        "expose",
        "network_mode",
        "networks",
        "privileged",
        "cap_add",
        "cap_drop",
        "security_opt",
        "devices",
        "pid",
        "ipc",
        "uts",
        "user",
        "userns_mode",
        "container_name",
        "hostname",
        "labels",
        "env_file",
        "extends",
        "include",
        "secrets",
        "configs",
        "volumes_from",
        "tmpfs",
        "extra_hosts",
        "dns",
        "sysctls",
        "deploy",
        "restart",
        "healthcheck",
        "pull_policy",
        "profiles",
        "x-custom",
        "unknown",
    ] {
        rejects(&format!(
            "services: {{api: {{image: 'api:1', {field}: null}}}}"
        ));
    }
    for field in [
        "version", "name", "networks", "include", "secrets", "configs", "x-custom",
    ] {
        rejects(&format!(
            "services: {{api: {{image: 'api:1'}}}}\n{field}: null"
        ));
    }
    for input in [
        "",
        "null",
        "[]",
        "{}",
        "services: {}",
        "services: null",
        "services: []",
        "services: {api: null}",
        "services: {api: []}",
        "services: {api: {image: null}}",
        "services: {api: {image: 12}}",
        "services: {api: {image: []}}",
    ] {
        assert_eq!(parse(input).unwrap_err(), ComposeError::Structure);
    }
}

#[test]
fn logical_service_names_and_count_are_bounded() {
    for name in [
        "",
        "browser",
        "pithos-app",
        "Api",
        "1api",
        "-api",
        "api_db",
        "a.b",
        "a/b",
        "é",
        &"a".repeat(41),
    ] {
        rejects(&format!("services: {{'{name}': {{image: 'api:1'}}}}"));
    }
    for name in ["a", "api-2", &"a".repeat(40)] {
        assert!(parse(&format!("services: {{'{name}': {{image: 'api:1'}}}}")).is_ok());
    }
    let mut input = String::from("services:\n");
    for n in 0..32 {
        input.push_str(&format!("  svc-{n}: {{image: 'api:1'}}\n"));
    }
    assert_eq!(parse(&input).unwrap().services().len(), 32);
    input.push_str("  excess: {image: 'api:1'}\n");
    rejects(&input);
}

#[test]
fn ambiguous_yaml_is_rejected_before_model_loading() {
    for input in [
        "services: {api: {image: 'api:1', image: 'api:2'}}",
        "services: {api: {image: 'api:1', \"im\\u0061ge\": 'api:2'}}",
        "services: {api: {image: 'api:1'}, api: {image: 'api:2'}}",
        "services: {api: {image: 'api:1'}}\nservices: {db: {image: 'db:1'}}",
        "services: {api: &a {image: 'api:1'}, db: *a}",
        "services: {api: {image: &a 'api:1'}}",
        "services: {api: {<<: {image: 'api:1'}, image: 'api:2'}}",
        "services: {api: !!map {image: 'api:1'}}",
        "services: {api: {image: !!str 'api:1'}}",
        "services: {api: {image: !private 'api:1'}}",
        "services: {api: {image: 'api:1'}}\n---\nservices: {db: {image: 'db:1'}}",
        "services: {api: {image: 'api:1'}}\n---\n",
        "services: {api: {image: 'api:1', ? [a, b]: value}}",
        "services: {api: {image: 'api:1', 1: a, 01: b}}",
    ] {
        rejects(input);
    }
    assert!(parse("---\nservices: {api: {image: 'api:1'}}\n...\n").is_ok());
}

#[test]
fn byte_depth_and_event_budgets_apply_before_loading() {
    let valid = "services: {api: {image: 'api:1'}}\n#";
    let exact = format!("{valid}{}", "x".repeat(65_536 - valid.len()));
    assert!(parse(&exact).is_ok());
    assert_eq!(parse(&(exact + "x")).unwrap_err(), ComposeError::Limit);
    // A depth-16 non-model is a schema error; the 17th collection exceeds preflight.
    assert_eq!(
        parse(&("[".repeat(16) + &"]".repeat(16))).unwrap_err(),
        ComposeError::Structure
    );
    assert_eq!(
        parse(&("[".repeat(17) + &"]".repeat(17))).unwrap_err(),
        ComposeError::Limit
    );
    // Scanner lookahead can hit its own bounded recursion guard before an event.
    assert!(matches!(
        parse(&("[".repeat(10_000) + &"]".repeat(10_000))),
        Err(ComposeError::Limit | ComposeError::Yaml)
    ));
    // Stream/document/sequence boundaries add six events to these scalar events.
    let events = |n| format!("[{}]", vec!["x"; n].join(","));
    assert_eq!(parse(&events(4090)).unwrap_err(), ComposeError::Structure);
    assert_eq!(parse(&events(4091)).unwrap_err(), ComposeError::Limit);
}

#[test]
fn images_are_literal_bounded_explicit_references() {
    for image in [
        "",
        "postgres",
        "${IMAGE}:1",
        "$IMAGE:1",
        "api:$$TAG",
        "api:1 secret",
        "-api:1",
        "https://host/api:1",
        "/api:1",
        "api/:1",
        "api::1",
        "api:",
        "api:../tag",
        "api@sha256:123",
        "api@md5:abcd",
        "../api:1",
        "api:1\\n",
        "UPPER/repo:1",
        &"a".repeat(513),
    ] {
        rejects(&format!("services: {{api: {{image: '{image}'}}}}"));
    }
    for image in [
        "postgres:16",
        "mcr.microsoft.com/dotnet/aspnet:8.0",
        "localhost:5000/team/api:dev-1",
        &format!("registry/team/api@sha256:{}", "a".repeat(64)),
        &format!("registry/team/api:1@sha256:{}", "b".repeat(64)),
    ] {
        assert_eq!(
            parse(&format!("services: {{api: {{image: '{image}'}}}}"))
                .unwrap()
                .services()["api"]
                .image(),
            Some(image)
        );
    }
}

#[test]
fn build_is_exclusive_and_paths_are_lexically_project_relative() {
    for build in [
        "'./api'",
        "{context: './api'}",
        "{context: './api', dockerfile: 'Dockerfile'}",
    ] {
        let model = parse(&format!("services: {{api: {{build: {build}}}}}")).unwrap();
        let service = &model.services()["api"];
        assert_eq!(service.image(), None);
        assert_eq!(service.build().unwrap().context(), "./api");
        assert_eq!(service.build().unwrap().dockerfile(), "Dockerfile");
    }
    let model =
        parse("services: {api: {build: {context: '.', dockerfile: 'docker/Api.Dockerfile'}}}")
            .unwrap();
    assert_eq!(
        model.services()["api"].build().unwrap().dockerfile(),
        "docker/Api.Dockerfile"
    );
    for build in [
        "null",
        "[]",
        "{}",
        "{dockerfile: Dockerfile}",
        "{context: null}",
        "{context: '.', args: {TOKEN: secret}}",
        "{context: '.', target: prod}",
        "{context: '.', ssh: default}",
        "{context: '.', additional_contexts: []}",
    ] {
        rejects(&format!("services: {{api: {{build: {build}}}}}"));
    }
    rejects("services: {api: {}}");
    rejects("services: {api: {image: 'api:1', build: '.'}}");
    for path in [
        "",
        "/tmp",
        "..",
        "../api",
        "api/../../escape",
        "api/../safe",
        "~/api",
        "C:/api",
        "C:\\api",
        "\\\\host\\api",
        "https://host/repo",
        "git@host:repo",
        "${ROOT}/api",
        "api\\child",
        "api//child",
        &"a".repeat(1025),
    ] {
        rejects(&format!(
            "services: {{api: {{build: {{context: '{path}'}}}}}}"
        ));
        rejects(&format!(
            "services: {{api: {{build: {{context: '.', dockerfile: '{path}'}}}}}}"
        ));
    }
    rejects("services: {api: {build: {context: '.', dockerfile: '.'}}}");
    // Existence and symlink resolution are deliberately not checked by this pure policy.
    assert!(parse("services: {api: {build: 'does-not-exist/child'}}").is_ok());
}

#[test]
fn command_and_entrypoint_are_bounded_literal_argv() {
    let model = parse(
        "services: {api: {image: 'api:1', command: ['dotnet', 'Api.dll', ''], entrypoint: []}}",
    )
    .unwrap();
    assert_eq!(
        model.services()["api"].command().unwrap(),
        ["dotnet", "Api.dll", ""]
    );
    assert_eq!(model.services()["api"].entrypoint().unwrap().len(), 0);
    let absent = parse("services: {api: {image: 'api:1'}}").unwrap();
    assert!(absent.services()["api"].command().is_none());
    assert!(absent.services()["api"].entrypoint().is_none());
    for field in ["command", "entrypoint"] {
        for value in [
            "null",
            "'sh -c whoami'",
            "{}",
            "[42]",
            "[true]",
            "['']",
            "['echo', '$HOME']",
            "['echo', '${SECRET}']",
            "['echo', '$$SECRET']",
            "[\"a\\0b\"]",
        ] {
            rejects(&format!(
                "services: {{api: {{image: 'api:1', {field}: {value}}}}}"
            ));
        }
        let argv = |n, size| {
            format!(
                "services: {{api: {{image: 'api:1', {field}: [{}]}}}}",
                vec![format!("'{}'", "x".repeat(size)); n].join(",")
            )
        };
        assert!(parse(&argv(64, 1)).is_ok());
        rejects(&argv(65, 1));
        assert!(parse(&argv(1, 4096)).is_ok());
        rejects(&argv(1, 4097));
    }
}

#[test]
fn environment_is_a_bounded_literal_string_map() {
    let model = parse("services: {api: {image: 'api:1', environment: {DATABASE_URL: 'Host=db;Password=secret', EMPTY: '', QUOTED: 'true', _PORT: '5432', MULTILINE: \"one\\ntwo\"}}}").unwrap();
    let env = model.services()["api"].environment();
    assert_eq!(env["DATABASE_URL"], "Host=db;Password=secret");
    assert_eq!(env["EMPTY"], "");
    assert_eq!(env["QUOTED"], "true");
    assert_eq!(env["MULTILINE"], "one\ntwo");
    for value in [
        "null",
        "[TOKEN]",
        "['TOKEN=secret']",
        "{TOKEN: null}",
        "{TOKEN: true}",
        "{TOKEN: 42}",
        "{TOKEN: {nested: secret}}",
        "{TOKEN: '${HOST_TOKEN}'}",
        "{TOKEN: '$TOKEN'}",
        "{TOKEN: '$$TOKEN'}",
        "{TOKEN: \"a\\0b\"}",
        "{'': value}",
        "{'A=B': value}",
        "{'1A': value}",
        "{'A-B': value}",
        "{'é': value}",
    ] {
        rejects(&format!(
            "services: {{api: {{image: 'api:1', environment: {value}}}}}"
        ));
    }
    let environment = |n| {
        format!(
            "services: {{api: {{image: 'api:1', environment: {{{}}}}}}}",
            (0..n)
                .map(|n| format!("VAR_{n}: ''"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    assert!(parse(&environment(128)).is_ok());
    rejects(&environment(129));
    for (key_len, value_len, accepted) in [(128, 4096, true), (129, 1, false), (1, 4097, false)] {
        let input = format!(
            "services: {{api: {{image: 'api:1', environment: {{'{}': '{}'}}}}}}",
            "A".repeat(key_len),
            "x".repeat(value_len)
        );
        assert_eq!(parse(&input).is_ok(), accepted);
    }
}

#[test]
fn dependencies_are_short_declared_unique_and_acyclic() {
    let model = parse("services:\n  api: {image: 'api:1', depends_on: [db, cache]}\n  db: {image: 'db:1', depends_on: [store]}\n  cache: {image: 'cache:1', depends_on: [store]}\n  store: {image: 'store:1'}").unwrap();
    assert_eq!(model.services()["api"].depends_on(), ["db", "cache"]);
    for value in [
        "null",
        "db",
        "{db: {condition: service_healthy}}",
        "[42]",
        "[missing]",
        "[api]",
        "[db, db]",
        "[browser]",
        "[pithos-app]",
    ] {
        rejects(&format!(
            "services: {{api: {{image: 'api:1', depends_on: {value}}}, db: {{image: 'db:1'}}}}"
        ));
    }
    rejects(
        "services: {api: {image: 'api:1', depends_on: [db]}, db: {image: 'db:1', depends_on: [api]}}",
    );
    rejects(
        "services: {api: {image: 'api:1'}, a: {image: 'a:1', depends_on: [b]}, b: {image: 'b:1', depends_on: [c]}, c: {image: 'c:1', depends_on: [a]}}",
    );
    let mut chain = String::from("services:\n  svc-0: {image: 'api:1', depends_on: []}\n");
    for n in 1..32 {
        chain.push_str(&format!(
            "  svc-{n}: {{image: 'api:1', depends_on: [svc-{}]}}\n",
            n - 1
        ));
    }
    assert!(parse(&chain).is_ok());
    rejects(&format!(
        "services: {{api: {{image: 'api:1', depends_on: [{}]}}, db: {{image: 'db:1'}}}}",
        vec!["db"; 33].join(",")
    ));
}

#[test]
fn volume_declarations_have_only_bounded_logical_identities() {
    let model =
        parse("services: {db: {image: 'postgres:16'}}\nvolumes: {db-data: {}, cache: null}")
            .unwrap();
    assert_eq!(
        model
            .volumes()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["cache", "db-data"]
    );
    for value in [
        "null",
        "[]",
        "{data: []}",
        "{data: 'disk'}",
        "{data: {external: true}}",
        "{data: {external: false}}",
        "{data: {name: foreign}}",
        "{data: {driver: local}}",
        "{data: {driver_opts: {device: /etc}}}",
        "{data: {labels: {x: y}}}",
        "{browser: {}}",
        "{pithos-app: {}}",
        "{Data: {}}",
        "{'../data': {}}",
    ] {
        rejects(&format!(
            "services: {{db: {{image: 'postgres:16'}}}}\nvolumes: {value}"
        ));
    }
    let declarations = |n| {
        format!(
            "services: {{db: {{image: 'postgres:16'}}}}\nvolumes: {{{}}}",
            (0..n)
                .map(|n| format!("data-{n}: {{}}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    assert!(parse(&declarations(0)).is_ok());
    assert!(parse(&declarations(32)).is_ok());
    rejects(&declarations(33));
    rejects(&format!(
        "services: {{db: {{image: 'postgres:16'}}}}\nvolumes: {{'{}': {{}}}}",
        "a".repeat(41)
    ));
}

#[test]
fn mounts_are_declared_named_volumes_with_unambiguous_absolute_targets() {
    let input = "services: {db: {image: 'postgres:16', volumes: ['data:/var/lib/postgresql/data', 'data:/backup:ro', 'data:/cache:rw']}}\nvolumes: {data: {}}";
    let model = parse(input).unwrap();
    let mounts = model.services()["db"].volumes();
    assert_eq!(mounts.len(), 3);
    assert_eq!(mounts[0].source(), "data");
    assert_eq!(mounts[0].target(), "/var/lib/postgresql/data");
    assert!(!mounts[0].read_only());
    assert!(mounts[1].read_only());
    assert!(!mounts[2].read_only());
    for value in [
        "null",
        "{}",
        "'data:/data'",
        "[42]",
        "[{type: volume, source: data, target: /data}]",
        "['/etc:/data']",
        "['./host:/data']",
        "['../host:/data']",
        "['~/host:/data']",
        "['C:\\host:/data']",
        "['/var/run/docker.sock:/var/run/docker.sock']",
        "['/data']",
        "['data']",
        "[': /data']",
        "['missing:/data']",
        "['data:relative']",
        "['data:/']",
        "['data:/a/../b']",
        "['data:/a/./b']",
        "['data://data']",
        "['data:/data/']",
        "['data:/data:z']",
        "['data:/data:RO']",
        "['data:/data:']",
        "['data:/data:ro:rw']",
        "['data:/data:ro,z']",
        "['data:/${TARGET}']",
        "['data:/data', 'data:/data:ro']",
        "['data:/a\\b']",
        "[\"data:/a\\0b\"]",
    ] {
        rejects(&format!(
            "services: {{db: {{image: 'postgres:16', volumes: {value}}}}}\nvolumes: {{data: {{}}}}"
        ));
    }
    let mounts = |n| {
        format!(
            "services: {{db: {{image: 'postgres:16', volumes: [{}]}}}}\nvolumes: {{data: {{}}}}",
            (0..n)
                .map(|n| format!("'data:/target-{n}'"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    assert!(parse(&mounts(32)).is_ok());
    rejects(&mounts(33));
    for (len, accepted) in [(1024, true), (1025, false)] {
        let input = format!(
            "services: {{db: {{image: 'postgres:16', volumes: ['data:/{}']}}}}\nvolumes: {{data: {{}}}}",
            "a".repeat(len - 1)
        );
        assert_eq!(parse(&input).is_ok(), accepted);
    }
}

#[test]
fn model_debug_and_all_error_channels_redact_input_values() {
    let secret = "sensitive-canary";
    let input = format!(
        "services: {{{secret}: {{build: {{context: '{secret}', dockerfile: '{secret}'}}, command: ['{secret}'], entrypoint: ['{secret}'], environment: {{TOKEN: '{secret}'}}, volumes: ['{secret}:/{secret}']}}}}\nvolumes: {{{secret}: {{}}}}"
    );
    let model = parse(&input).unwrap();
    let service = &model.services()[secret];
    let debug = format!(
        "{model:?} {model:#?} {service:?} {:?} {:?}",
        service.build().unwrap(),
        service.volumes()[0]
    );
    assert!(!debug.contains(secret), "Debug leaked caller data");
    assert_eq!(
        service.environment()["TOKEN"],
        secret,
        "getters retain literal values"
    );
    let image_model = parse(&format!("services: {{api: {{image: '{secret}:1'}}}}")).unwrap();
    assert!(!format!("{image_model:?} {:?}", image_model.services()["api"]).contains(secret));
    for input in [
        format!("services: {{api: {{image: '{secret}:1', '{secret}': true}}}}"),
        format!("services: {{api: {{image: '{secret}:1', image: '{secret}:2'}}}}"),
        format!("services: {{api: {{image: \"\\q{secret}\"}}}}"),
        format!("services: {{api: {{image: *{secret}}}}}"),
        format!("services: {{api: {{image: '{secret}:1', environment: {{TOKEN: '${secret}'}}}}}}"),
        format!("services: {{api: {{build: '../{secret}'}}}}"),
    ] {
        let error = parse(&input).unwrap_err();
        assert!(!format!("{error} {error:?} {error:#?}").contains(secret));
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn realistic_dotnet_api_and_database_preserve_only_the_approved_model() {
    let input = r#"
services:
  api:
    build:
      context: ./src/Api
      dockerfile: docker/Dockerfile
    entrypoint: [dotnet]
    command: [Api.dll, --urls, 'http://0.0.0.0:8080']
    environment:
      ASPNETCORE_ENVIRONMENT: Development
      ConnectionStrings__Main: 'Host=db;Database=app;Username=app;Password=local-test-secret'
    depends_on: [db]
  db:
    image: postgres:16.4
    environment:
      POSTGRES_DB: app
      POSTGRES_USER: app
      POSTGRES_PASSWORD: local-test-secret
    volumes: ['db-data:/var/lib/postgresql/data:rw']
  reporter:
    image: example/reporter:1
    depends_on: [db]
    volumes: ['db-data:/snapshot:ro']
volumes:
  db-data: {}
"#;
    let model = parse(input).unwrap();
    assert_eq!(model.services().len(), 3);
    assert_eq!(model.volumes().len(), 1);
    let api = &model.services()["api"];
    assert_eq!(api.build().unwrap().context(), "./src/Api");
    assert_eq!(api.build().unwrap().dockerfile(), "docker/Dockerfile");
    assert_eq!(api.entrypoint().unwrap(), ["dotnet"]);
    assert_eq!(
        api.command().unwrap(),
        ["Api.dll", "--urls", "http://0.0.0.0:8080"]
    );
    assert_eq!(api.depends_on(), ["db"]);
    assert_eq!(
        api.environment()["ConnectionStrings__Main"],
        "Host=db;Database=app;Username=app;Password=local-test-secret"
    );
    assert_eq!(model.services()["db"].image(), Some("postgres:16.4"));
    assert!(!model.services()["db"].volumes()[0].read_only());
    assert!(model.services()["reporter"].volumes()[0].read_only());
    assert!(!format!("{model:#?}").contains("local-test-secret"));
}

#[test]
fn escaped_duplicates_and_malformed_inputs_fail_closed() {
    for input in [
        "services: {api: {image: 'api:1', environment: {TOKEN: one, \"TO\\u004bEN\": two}}}",
        "services: {api: {build: {context: '.', context: './elsewhere'}}}",
        "services: {api: {image: 'api:1'}}\nvolumes: {data: {}, data: {}}",
        "services: {api: {image: 'api:1', environment: {'<<': secret}}}",
        "services: {api: {image: 'api:1', command: [&a echo, *a]}}",
        "services: {api: {image: 'api:1', environment: !!map {TOKEN: secret}}}",
        "services: {api: {image: 'api:1'}}\n---\n[]",
        "services: {api: {image: 'api:1'}",
        "services:\n\tapi: {image: 'api:1'}",
        "\0",
    ] {
        rejects(input);
    }
    // Syntax-looking characters in literal strings are data, not YAML features.
    let model = parse(
        "services: {api: {image: 'api:1', command: ['echo', '*alias &anchor <<: --- !tag']}}",
    )
    .unwrap();
    assert_eq!(
        model.services()["api"].command().unwrap()[1],
        "*alias &anchor <<: --- !tag"
    );
}

#[test]
fn exact_string_and_name_boundaries_are_accepted() {
    let input = format!(
        "services: {{'{}': {{image: '{}:1'}}}}\nvolumes: {{'{}': {{}}}}",
        "a".repeat(40),
        "a".repeat(510),
        "v".repeat(40)
    );
    assert!(parse(&input).is_ok());
    let input = format!(
        "services: {{api: {{build: {{context: '{}', dockerfile: '{}'}}}}}}",
        "c".repeat(1024),
        "d".repeat(1024)
    );
    assert!(parse(&input).is_ok());
    // Count bytes, not Unicode scalar values, for literal string budgets.
    for (count, accepted) in [(2048, true), (2049, false)] {
        let input = format!(
            "services: {{api: {{image: 'api:1', environment: {{TOKEN: '{}'}}}}}}",
            "é".repeat(count)
        );
        assert_eq!(parse(&input).is_ok(), accepted);
    }
}

#[test]
fn overflowing_radix_integers_do_not_become_literal_strings() {
    for value in [
        "0x8000000000000000".to_owned(),
        "0xffffffffffffffff".to_owned(),
        "0o1000000000000000000000".to_owned(),
        format!("0x{}", "a".repeat(200)),
        format!("0o{}", "7".repeat(200)),
    ] {
        for input in [
            format!("services: {{api: {{image: 'api:1', environment: {{FLAG: {value}}}}}}}"),
            format!("services: {{api: {{image: 'api:1', command: [{value}]}}}}"),
            format!("services: {{api: {{build: {value}}}}}"),
        ] {
            rejects(&input);
        }
        let quoted = format!(
            "services: {{api: {{build: '{value}', command: ['{value}'], environment: {{FLAG: '{value}'}}}}}}"
        );
        let model = parse(&quoted).unwrap();
        assert_eq!(model.services()["api"].environment()["FLAG"], value);
    }
}

#[test]
fn ambiguous_plain_core_scalars_require_quotes_for_literal_strings() {
    for value in ["True", "TRUE", "False", "FALSE", "Null", "NULL"] {
        for input in [
            format!("services: {{api: {{image: 'api:1', environment: {{FLAG: {value}}}}}}}"),
            format!("services: {{api: {{image: 'api:1', command: [{value}]}}}}"),
            format!("services: {{api: {{build: {value}}}}}"),
        ] {
            rejects(&input);
        }
        let quoted = format!(
            "services: {{api: {{build: '{value}', command: ['{value}'], environment: {{FLAG: '{value}'}}}}}}"
        );
        let model = parse(&quoted).unwrap();
        assert_eq!(model.services()["api"].build().unwrap().context(), value);
        assert_eq!(model.services()["api"].command().unwrap(), [value]);
        assert_eq!(model.services()["api"].environment()["FLAG"], value);
    }
}

#[test]
fn raw_nul_cannot_hide_trailing_unvalidated_yaml() {
    for suffix in [
        "",
        "\n---\nservices: {evil: {privileged: true}}",
        "\nmalformed: [",
    ] {
        let input = format!("services: {{api: {{image: 'api:1'}}}}\0{suffix}");
        assert_eq!(parse(&input).unwrap_err(), ComposeError::Yaml);
    }
}

#[test]
fn minimal_image_service_is_an_owned_typed_model() {
    let model = parse("services: {api: {image: 'example/api:1.0'}}").unwrap();
    assert_eq!(model.services().len(), 1);
    assert_eq!(model.services()["api"].image(), Some("example/api:1.0"));
}
