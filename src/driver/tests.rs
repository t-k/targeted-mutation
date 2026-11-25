use crate::instrument::LiteralSide;

use super::*;

#[test]
fn a_target_is_selected_by_the_first_of_its_kinds() {
    let target = |kinds: &[&str]| Target {
        name: "tool".to_string(),
        kind: kinds.iter().map(|kind| kind.to_string()).collect(),
        src_path: PathBuf::from("src/lib.rs"),
        test: true,
        doctest: false,
        required_features: vec![],
    };
    assert_eq!(
        target(&["lib", "example"]).test_build_arg(),
        Some(("--lib", None))
    );
    assert_eq!(
        target(&["example", "lib"]).test_build_arg(),
        Some(("--example", Some("tool")))
    );
    assert_eq!(target(&[]).test_build_arg(), None);
}

#[test]
fn code_is_left_out_only_where_every_build_leaves_it_out() {
    // One build with the feature and one without: what asks for it and what asks
    // for its absence are each built by one of them.
    let with = vec![(10, 20)];
    let without = vec![(30, 40)];
    assert_eq!(left_out_of_all(&[with.clone(), without.clone()]), []);
    assert_eq!(left_out_of_all(&[with.clone(), with.clone()]), [(10, 20)]);
    assert_eq!(left_out_of_all(std::slice::from_ref(&with)), [(10, 20)]);
    // An item one build leaves out inside a module the other leaves out.
    assert_eq!(
        left_out_of_all(&[vec![(0, 50)], vec![(10, 20)]]),
        [(10, 20)]
    );
    assert_eq!(
        left_out_of_all(&[vec![(0, 50), (60, 70)], vec![(10, 20), (60, 70)]]),
        [(10, 20), (60, 70)]
    );
    assert_eq!(left_out_of_all(&[]), []);
    assert_eq!(left_out_of_all(&[vec![], with]), []);
}

#[test]
fn a_configuration_that_passes_a_feature_to_the_compiler_is_seen() {
    let dir = scratch("config-feature");
    let inner = dir.join("a/b");
    fs::create_dir_all(&inner).unwrap();
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    let asks = |home: &Path| configuration_names_a_feature(&inner, Some(home.to_path_buf()));
    assert!(!asks(&home));
    // In a directory above the build, in either file name, and in cargo's home.
    for (place, name) in [
        (dir.join("a/.cargo"), "config.toml"),
        (dir.join(".cargo"), "config"),
    ] {
        fs::create_dir_all(&place).unwrap();
        fs::write(
            place.join(name),
            "[build]\nrustflags = [\"--cfg\", 'feature=\"extra\"']\n",
        )
        .unwrap();
        assert!(asks(&home), "{name}");
        fs::remove_dir_all(&place).unwrap();
    }
    fs::write(
        home.join("config.toml"),
        "[build]\nrustflags = \"--cfg feature=\\\"x\\\"\"\n",
    )
    .unwrap();
    assert!(asks(&home));
    // A target table and a flag that names no feature are not that.
    fs::write(
        home.join("config.toml"),
        "[target.'cfg(unix)']\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n",
    )
    .unwrap();
    assert!(!asks(&home));
}

#[test]
fn flags_that_may_name_a_feature_make_the_features_unknown() {
    let flag = |text: &str| Some(std::ffi::OsString::from(text));
    assert!(features_from_flags([flag("--cfg feature=\"extra\"")]));
    assert!(features_from_flags([
        None,
        flag("-C opt-level=1\u{1f}--cfg\u{1f}feature=\"x\"")
    ]));
    assert!(!features_from_flags([flag("-C debuginfo=0"), None]));
    assert!(!features_from_flags([flag(
        "-C target-feature=+crt-static"
    )]));
    assert!(!features_from_flags([flag(
        "--cfg tokio_unstable -C target-feature=+avx2"
    )]));
    assert!(features_from_flags([flag(
        "-C target-feature=+avx2 --cfg feature=\"x\""
    )]));
    assert!(!features_from_flags([None, None, None]));
}
use crate::trace::UNKNOWN_TEST;

#[test]
fn explicit_timeout_conflict_matches_the_coverage_table() {
    for row in include_str!("../../tests/data/timeout-baseline.pict.tsv")
        .lines()
        .skip(1)
    {
        let columns: Vec<_> = row.split('\t').collect();
        let &[limit, baseline, selection] = columns.as_slice() else {
            panic!("invalid PICT row: {row}");
        };
        let timeout = (limit == "explicit").then_some(Duration::from_secs(1));
        let measured = Duration::from_millis(match baseline {
            "below" => 999,
            "equal" => 1_000,
            "above" => 1_001,
            _ => panic!("invalid baseline: {baseline}"),
        });
        let selected_count = match selection {
            "full" => None,
            "all-names" => Some(2),
            "filtered" => Some(1),
            _ => panic!("invalid selection: {selection}"),
        };
        let expected = limit == "explicit" && baseline != "below" && selection != "filtered";
        assert_eq!(
            explicit_timeout_conflict(timeout, measured, selected_count, 2).is_some(),
            expected,
            "{row}"
        );
    }
}

#[test]
fn campaign_phase_combinations() {
    let rows = include_str!("../../tests/data/campaign-phases.pict.tsv");
    for line in rows.lines().skip(1) {
        let columns: Vec<_> = line.split('\t').collect();
        let &[baselines, workers, policy, verdict] = columns.as_slice() else {
            panic!("invalid PICT row: {line}");
        };
        let count = |value| if value == "one" { 1 } else { 2 };
        let ready = std::sync::Barrier::new(count(baselines));
        let baselines = run_pool(2, count(baselines), |index| {
            ready.wait();
            Ok(index)
        })
        .unwrap();
        assert_eq!(baselines, [0, 1]);

        let serial = policy == "serial";
        let workers = count(workers);
        let results = evaluate_jobs(
            2,
            workers,
            serial,
            false,
            "test",
            None,
            &|index, sharing| {
                let time_out =
                    index == 0 && verdict != "pass" && (sharing > 1 || verdict == "timeout");
                let unable = index == 0 && verdict == "unable" && sharing == 1;
                Ok(JobResult {
                    first_pass_timeout: false,
                    unconfirmed_timeout: false,
                    survived: (!time_out && !unable).then(|| vec![index.to_string()]),
                    timed_out: time_out,
                    unable,
                })
            },
        )
        .unwrap();
        assert_eq!(results[1].survived.as_deref(), Some(&["1".to_owned()][..]));
        let mut stats = report::TimeoutRechecks::default();
        stats.include(&results[0]);
        let first_pass = usize::from(workers == 2 && verdict != "pass");
        assert_eq!(stats.first_pass, first_pass, "{line}");
        assert_eq!(
            stats.confirmed,
            usize::from(first_pass == 1 && serial && verdict == "timeout"),
            "{line}"
        );
        assert_eq!(
            stats.recovered,
            usize::from(first_pass == 1 && serial && verdict == "recovered"),
            "{line}"
        );
        assert_eq!(
            stats.unconfirmed,
            usize::from(first_pass == 1 && (!serial || verdict == "unable")),
            "{line}"
        );
    }
}

#[cfg(unix)]
#[test]
fn mutant_timeout_uses_the_measured_filter_and_an_explicit_override_wins() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("filtered-timeout");
    let exe = dir.join("runner");
    fs::write(
        &exe,
        "#!/bin/sh\n[ \"$2\" = --exact ] && [ \"$3\" = fast ]\n",
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let mut bin = TestBinary {
        name: "tests".into(),
        cwd: dir.clone(),
        marks: marks_dir(&dir).unwrap(),
        git_ceiling: dir.clone(),
        work_lock: None,
        tests: ["fast".to_string(), "slow".to_string()]
            .into_iter()
            .collect(),
        runner: Runner::Exe(exe),
        baseline: Duration::from_secs(70),
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::from_secs(70),
        timeout: None,
        wrapper_grace: Duration::ZERO,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    };
    // Whatever the run of `fast` took on this machine, it is less than the 70 seconds
    // of the whole binary: a bound in seconds would fail on a loaded machine.
    assert!(bin.filtered_limit(Some(&["fast".to_string()]), false, 1) < Duration::from_secs(355));
    assert_eq!(
        bin.filtered_limit(Some(&["slow".to_string()]), false, 1),
        Duration::from_secs(355)
    );
    assert_eq!(bin.filtered_limit(None, false, 1), Duration::from_secs(355));
    assert_eq!(bin.filtered_timings.lock().unwrap().len(), 2);
    bin.timeout = Some(Duration::from_secs(8));
    assert_eq!(
        bin.filtered_limit(Some(&["fast".to_string()]), false, 1),
        Duration::from_secs(8)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_build_retains_spanless_and_source_diagnostics() {
    let stdout = r#"
{"reason":"compiler-message","message":{"level":"error","message":"linking with cc failed","rendered":"ld: library not found for -liconv","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"source error","rendered":null,"spans":[{"file_name":"src/lib.rs","byte_start":1,"line_start":2,"is_primary":true}]}}
"#;
    let failed = parse_build_output(stdout, &["p"]);
    assert_eq!(failed.errors.len(), 1);
    let text = failed.diagnostics.join("\n");
    assert!(text.contains("ld: library not found for -liconv"), "{text}");
    assert!(text.contains("source error"), "{text}");
}

#[test]
fn test_build_scope_uses_only_known_local_features() {
    let package = Package {
        name: "pkg".to_owned(),
        dependencies: vec![],
        features: BTreeMap::from([
            ("default".to_owned(), vec!["alias".to_owned()]),
            (
                "alias".to_owned(),
                vec!["extra".to_owned(), "dep/extra".to_owned()],
            ),
            ("extra".to_owned(), vec![]),
            ("separate".to_owned(), vec![]),
        ]),
        manifest_path: PathBuf::from("workspace/pkg/Cargo.toml"),
        targets: vec![],
    };
    let enabled = |workspace: &str, features: &str, no_default_features, all_features| {
        package.enabled_local_features(
            Path::new(workspace),
            &CargoConfig {
                features: vec![features.to_owned()],
                no_default_features,
                all_features,
                locked: false,
            },
        )
    };
    let root = "workspace/pkg";
    assert_eq!(
        enabled(root, "", false, false),
        BTreeSet::from(["default", "alias", "extra"])
    );
    assert_eq!(
        enabled(root, "", true, true),
        BTreeSet::from(["default", "alias", "extra", "separate"])
    );
    assert_eq!(
        enabled(root, "alias, separate", true, false),
        BTreeSet::from(["alias", "extra", "separate"])
    );
    assert_eq!(
        enabled("workspace", "extra other/separate", true, false),
        BTreeSet::new()
    );
    assert_eq!(
        enabled("workspace", "pkg/extra other/separate", true, false),
        BTreeSet::from(["extra"])
    );
}

#[test]
fn test_build_scope_preserves_cargo_target_eligibility() {
    let target = |name, kind, test, required_features: &[&str]| {
        serde_json::from_value(serde_json::json!({
            "name": name, "kind": [kind], "test": test, "src_path": "ignored.rs",
            "required-features": required_features,
        }))
        .unwrap()
    };
    let package = |name: &str, targets| Package {
        name: name.to_owned(),
        dependencies: vec![],
        targets,
        manifest_path: PathBuf::new(),
        features: BTreeMap::from([("extra".to_owned(), vec![])]),
    };
    let first = package(
        "first",
        vec![
            target("first", "proc-macro", true, &[]),
            target("name", "bin", true, &[]),
            target("name", "example", true, &[]),
            target("name", "bench", true, &[]),
            target("name", "test", true, &[]),
            target("conditional", "test", true, &["extra"]),
            target("dependency_feature", "test", true, &["dep/extra"]),
            target("disabled", "test", false, &[]),
            target("glob[a]", "test", true, &[]),
            target("--leading-dash", "test", true, &[]),
            target("build-script-build", "custom-build", false, &[]),
        ],
    );
    let second = package(
        "second",
        vec![
            target("second", "staticlib", true, &[]),
            target("name", "test", false, &[]),
        ],
    );
    let flags = |packages: &[&Package], names: &[&str], all| {
        let selection = TestSelection {
            targets: names.iter().map(|s| s.to_string()).collect(),
            ..TestSelection::default()
        };
        test_build_flags(
            packages,
            &selection,
            Path::new("workspace"),
            &CargoConfig {
                all_features: all,
                ..CargoConfig::default()
            },
        )
    };
    assert_eq!(
        flags(&[&first], &["name", "first", "unknown"], false).unwrap(),
        [
            "--bench=name",
            "--bin=name",
            "--example=name",
            "--lib",
            "--test=name"
        ]
    );
    assert_eq!(flags(&[&first], &[], false), None);
    assert_eq!(flags(&[&first], &["unknown", "disabled"], false), None);
    assert_eq!(flags(&[&first], &["glob[a]"], false), None);
    assert_eq!(
        flags(&[&first], &["--leading-dash"], false).unwrap(),
        ["--test=--leading-dash"]
    );
    assert_eq!(flags(&[&first], &["name", "conditional"], false), None);
    assert_eq!(flags(&[&first], &["dependency_feature"], true), None);
    assert_eq!(
        flags(&[&first], &["conditional"], true).unwrap(),
        ["--test=conditional"]
    );
    assert_eq!(flags(&[&first, &second], &["name"], false), None);
    assert_eq!(flags(&[&first, &second], &["first"], false), None);
    assert_eq!(
        flags(&[&first, &second], &["first", "second"], false).unwrap(),
        ["--lib"]
    );
}

#[test]
fn parses_cargo_json_messages() {
    let stdout = concat!(
        r#"{"reason":"compiler-artifact","package_id":"p 0.1.0","target":{"name":"p","kind":["lib"]},"profile":{"test":false},"executable":null}"#,
        "\n",
        r#"{"reason":"compiler-artifact","package_id":"p 0.1.0","target":{"name":"p","kind":["lib"]},"profile":{"test":true},"executable":"/t/p-abc"}"#,
        "\n",
        r#"{"reason":"compiler-artifact","package_id":"dep 1.0.0","target":{"name":"dep","kind":["lib"]},"profile":{"test":true},"executable":"/t/dep-abc"}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"w","spans":[{"file_name":"src/lib.rs","byte_start":1,"line_start":1,"is_primary":true}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"error","rendered":"E0308","spans":[{"file_name":"src/lib.rs","byte_start":40,"line_start":3,"is_primary":true},{"file_name":"src/lib.rs","byte_start":2,"line_start":1,"is_primary":false}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"error","rendered":"E0308","spans":[{"file_name":"src/lib.rs","byte_start":40,"line_start":3,"is_primary":true},{"file_name":"src/lib.rs","byte_start":2,"line_start":1,"is_primary":false}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"error","rendered":"aborting","spans":[]}}"#,
        "\n",
        "not json\n",
        r#"{"reason":"build-finished","success":false}"#,
        "\n",
    );
    let out = parse_build_output(stdout, &["p", "other"]);
    assert_eq!(
        out.test_binaries,
        [(
            "p".to_string(),
            PathBuf::from("/t/p-abc"),
            "p".to_string(),
            vec!["lib".to_string()]
        )]
    );
    // With several packages each binary is attributed to its own.
    let both = parse_build_output(stdout, &["dep", "p"]);
    let owners: Vec<&str> = both.test_binaries.iter().map(|b| b.2.as_str()).collect();
    assert_eq!(owners, ["dep", "p"]);
    assert_eq!(
        out.errors,
        [CompileError {
            code: None,
            message: String::new(),
            primary: vec![ErrorSpan {
                file: "src/lib.rs".into(),
                byte_start: 40,
                line: 3
            }],
            secondary: vec![ErrorSpan {
                file: "src/lib.rs".into(),
                byte_start: 2,
                line: 1
            }],
            rendered: "E0308".into(),
        }]
    );
}

#[test]
fn package_ids_are_matched_by_name() {
    assert!(is_package(
        "path+file:///w/target/tmt/src#eligibility@0.0.0",
        "eligibility"
    ));
    assert!(is_package(
        "path+file:///w/eligibility#0.1.0",
        "eligibility"
    ));
    assert!(is_package(
        "eligibility 0.1.0 (path+file:///w)",
        "eligibility"
    ));
    assert!(!is_package(
        "path+file:///w/eligibility#other@0.1.0",
        "eligibility-core"
    ));
    assert!(!is_package(
        "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
        "eligibility"
    ));
}

#[test]
fn test_filter_falls_back_to_the_whole_binary() {
    let set = |names: &[&str]| {
        names
            .iter()
            .map(|n| n.to_string())
            .collect::<BTreeSet<String>>()
    };
    let known = set(&["tests::a", "tests::b", "tests::c"]);
    assert_eq!(
        test_filter(&set(&["tests::a", "tests::b"]), &known),
        Some(vec!["tests::a".to_string(), "tests::b".to_string()])
    );
    // Observed outside any test thread.
    assert_eq!(test_filter(&set(&["tests::a", UNKNOWN_TEST]), &known), None);
    // Observed on a thread the test named itself: not a test of this binary.
    assert_eq!(test_filter(&set(&["tests::a", "worker"]), &known), None);
    assert_eq!(test_filter(&set(&["worker"]), &known), None);

    let many: BTreeSet<String> = (0..=MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
    assert_eq!(test_filter(&many, &many), None);
    let limit: BTreeSet<String> = (0..MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
    assert!(test_filter(&limit, &limit).is_some());
}

#[test]
fn libtest_listing() {
    let listing =
        "tests::a: test\ntests::nested::b: test\nbench_it: benchmark\n\n2 tests, 1 benchmark\n";
    assert_eq!(
        parse_test_list(listing).into_iter().collect::<Vec<_>>(),
        ["tests::a", "tests::nested::b"]
    );
}

#[test]
fn domains_from_type_bounds() {
    let u8s = crate::trace::TypeBounds {
        lhs: (0, 255),
        rhs: (0, 255),
    };
    assert_eq!(
        domain_of(LiteralSide::Rhs(18), Some(&u8s)),
        Some((-18, 237))
    );
    assert_eq!(
        domain_of(LiteralSide::Lhs(18), Some(&u8s)),
        Some((-237, 18))
    );
    assert_eq!(domain_of(LiteralSide::None, Some(&u8s)), None);
    assert_eq!(domain_of(LiteralSide::Rhs(18), None), None);
    let wide = crate::trace::TypeBounds {
        lhs: (i128::MIN, 0),
        rhs: (0, 0),
    };
    assert_eq!(
        domain_of(LiteralSide::Rhs(1), Some(&wide)),
        None,
        "overflow is not a domain"
    );
}

#[test]
fn a_primary_blame_does_not_inspect_later_fallbacks() {
    let copy = Path::new("/work");
    let source = "pub fn f(n: u32) -> bool { n >= 4 }";
    let make = |name: &str, rendered: bool| {
        let sites = instrument::discover(source, 0).unwrap().sites;
        let output = rendered.then(|| instrument::render(source, &sites, &|_| true));
        SourceFile {
            display: name.to_string(),
            path: copy.join(name),
            owners: BTreeSet::new(),
            lines: scope::LineIndex::new(source),
            original: source.to_string(),
            places: vec![],
            sites,
            rebuild_sites: vec![],
            rendered: output,
        }
    };
    let files = [make("primary.rs", true), make("secondary.rs", false)];
    let rendered = files[0].rendered.as_ref().unwrap();
    let probe = rendered.text.find("__tmt_rt::").unwrap();
    let span = |file: &str, byte_start| ErrorSpan {
        file: file.to_string(),
        byte_start,
        line: 1,
    };
    let error = CompileError {
        code: None,
        message: String::new(),
        primary: vec![span("primary.rs", probe)],
        secondary: vec![span("secondary.rs", 0)],
        rendered: String::new(),
    };
    assert_eq!(
        blame_error(&error, &files, copy, &HashSet::new()),
        [rendered.inserted_by(probe).unwrap()]
    );
}

#[test]
fn repair_reuses_a_render_but_still_corrects_the_working_copy() {
    let dir = scratch("render-repair");
    let root = dir.join("work/src");
    let path = root.join("f.rs");
    let state = WorkCopy::open(&dir.join("work"), &root);
    let source = "pub fn f(n: u32) -> bool { n >= 4 }";
    let suffix = "\n// root suffix\n";
    for row in include_str!("../../tests/data/repair-reuse.pict.tsv")
        .lines()
        .skip(1)
    {
        let columns: Vec<_> = row.split('\t').collect();
        let [cache, which, disk] = columns.as_slice() else {
            panic!("{row}")
        };
        let sites = instrument::discover(source, 1).unwrap().sites;
        let id = sites
            .iter()
            .find(|site| matches!(site.kind, SiteKind::Cmp { .. }))
            .unwrap()
            .id;
        let disabled = match *which {
            "here" => HashSet::from([id]),
            "elsewhere" => HashSet::from([u32::MAX]),
            _ => HashSet::new(),
        };
        let before = instrument::render(source, &sites, &|_| true);
        let expected = instrument::render(source, &sites, &|id| !disabled.contains(&id));
        let original_text = format!("{}{suffix}", before.text);
        fs::write(
            &path,
            if *disk == "rendered" {
                &original_text
            } else {
                source
            },
        )
        .unwrap();
        let mut file = SourceFile {
            display: "f.rs".to_string(),
            path: path.clone(),
            owners: BTreeSet::new(),
            lines: scope::LineIndex::new(source),
            original: source.to_string(),
            places: vec![],
            sites,
            rebuild_sites: vec![],
            rendered: (*cache == "present").then_some(before),
        };
        write_rendered(&mut file, &state, suffix, &disabled, &disabled).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}{suffix}", expected.text),
            "{row}"
        );
        let rendered = file.rendered.unwrap();
        assert_eq!(rendered.text, expected.text, "{row}");
        for offset in 0..rendered.text.len() {
            assert_eq!(
                rendered.to_original(offset),
                expected.to_original(offset),
                "{row}"
            );
            assert_eq!(
                rendered.inserted_by(offset),
                expected.inserted_by(offset),
                "{row}"
            );
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_graph_shares_edges_without_sharing_resolution_or_ownership() {
    let scratch = scratch("shared-source-graph");
    let copy = scratch.join("work/src");
    let before = copy.join("before.rs");
    let library = copy.join("lib.rs");
    let after = copy.join("after.rs");
    let shared = copy.join("shared.rs");
    let leaf = copy.join("leaf.rs");
    let test_helper = copy.join("test_helper.rs");
    let nested_leaf = copy.join("shared/leaf.rs");
    let sources = BTreeMap::from([
        (before.clone(), r#"#[path = "shared.rs"] mod dep;"#),
        (
            library.clone(),
            r#"mod shared; #[path = "shared.rs"] mod dep;"#,
        ),
        (after.clone(), r#"include!("shared.rs");"#),
        (shared.clone(), "mod leaf; #[cfg(test)] mod test_helper;"),
        (leaf.clone(), "fn top() {}"),
        (test_helper.clone(), "fn testing() {}"),
        (nested_leaf.clone(), "fn nested() {}"),
    ]);
    for path in sources.keys() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(path).unwrap();
    }
    let mut reads = BTreeMap::new();
    let graph = source_graph(
        [
            (before.clone(), false),
            (library.clone(), true),
            (after.clone(), false),
        ],
        &copy,
        |path| {
            *reads.entry(path.to_path_buf()).or_insert(0) += 1;
            Ok(sources.get(path).map(|source| (*source).to_string()))
        },
    )
    .unwrap();
    fs::remove_dir_all(scratch).unwrap();

    assert_eq!(
        reads[&shared], 2,
        "one load in each of two resolution contexts"
    );
    assert_eq!(reads[&leaf], 1);
    assert_eq!(reads[&test_helper], 1);
    assert!(!graph.contains_key(&test_helper));
    assert_eq!(reads[&nested_leaf], 1);
    for path in [&leaf, &shared] {
        assert_eq!(
            graph[path].consumers,
            BTreeSet::from([before.clone(), library.clone(), after.clone()])
        );
        assert_eq!(graph[path].production, BTreeSet::from([library.clone()]));
    }
    assert_eq!(
        graph[&nested_leaf].consumers,
        BTreeSet::from([library.clone()])
    );
    assert_eq!(graph[&nested_leaf].production, BTreeSet::from([library]));
    assert!(!graph.contains_key(&before) && !graph.contains_key(&after));
}

#[test]
fn out_of_line_module_locations() {
    let file = Path::new("src/cli.rs");
    let statements = "mod child; #[path = \"spec/support.rs\"] mod helper;";
    let roots = crate::modules::external(statements, file, Path::new("src"));
    assert_eq!(
        roots[0].paths,
        [
            PathBuf::from("src/child.rs"),
            PathBuf::from("src/child/mod.rs")
        ]
    );
    assert_eq!(roots[1].paths, [PathBuf::from("src/spec/support.rs")]);
    let module = crate::modules::external(statements, file, Path::new("src/cli"));
    assert_eq!(module[0].paths[0], PathBuf::from("src/cli/child.rs"));
    assert_eq!(module[1].paths[0], PathBuf::from("src/spec/support.rs"));
}

#[test]
fn path_dependencies_outside_the_workspace_become_absolute() {
    let manifest = r#"[package]
name = "app"

[dependencies]
serde = "1"
core = { path = "../core" }          # inside the workspace: kept
outside = { path = "../../outside", version = "1" }

[dev-dependencies.helper]
path = "../../../far/helper"

[target.'cfg(unix)'.dependencies]
unix-only = { path = "../../unix" }

[patch.crates-io]
serde = { path = "../../serde-fork" }
"#;
    let out = absolutize_path_dependencies(
        manifest,
        Path::new("/w/ws/app"),
        Path::new("/w/ws"),
        Path::new("/copy"),
    )
    .unwrap()
    .expect("something to rewrite");
    assert!(
        out.contains(r#"core = { path = "../core" }          # inside the workspace: kept"#),
        "{out}"
    );
    assert!(
        out.contains(r#"outside = { path = "/w/outside", version = "1" }"#),
        "{out}"
    );
    assert!(out.contains("path = \"/far/helper\""), "{out}");
    assert!(out.contains(r#"unix-only = { path = "/w/unix" }"#), "{out}");
    assert!(
        out.contains(r#"serde = { path = "/w/serde-fork" }"#),
        "{out}"
    );
    assert!(out.contains("serde = \"1\""), "{out}");

    let inside = "[dependencies]\ncore = { path = \"../core\" }\n[workspace.dependencies]\nx = { path = \"x\" }\n";
    assert_eq!(
        absolutize_path_dependencies(
            inside,
            Path::new("/w/ws/app"),
            Path::new("/w/ws"),
            Path::new("/copy")
        )
        .unwrap(),
        None
    );
    let ws = "[workspace.dependencies]\nshared = { path = \"../shared\" }\n";
    let out = absolutize_path_dependencies(
        ws,
        Path::new("/w/ws"),
        Path::new("/w/ws"),
        Path::new("/copy"),
    )
    .unwrap()
    .unwrap();
    assert!(out.contains(r#"shared = { path = "/w/shared" }"#), "{out}");
    assert!(
        absolutize_path_dependencies(
            "not [ toml",
            Path::new("/w"),
            Path::new("/w"),
            Path::new("/copy")
        )
        .is_err()
    );
    assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    // An absolute path into the workspace has to follow the code into the copy.
    let absolute = "[dependencies]\nhelper = { path = \"/w/ws/helper\" }\n";
    let out = absolutize_path_dependencies(
        absolute,
        Path::new("/w/ws/app"),
        Path::new("/w/ws"),
        Path::new("/copy"),
    )
    .unwrap()
    .unwrap();
    assert!(
        out.contains(r#"helper = { path = "/copy/helper" }"#),
        "{out}"
    );
}

#[test]
fn pool_runs_every_job_once_and_keeps_the_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for workers in [0, 1, 2, 3, 8, 100] {
        for count in [0, 1, 2, 7, 50] {
            let calls: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
            let results = run_pool(count, workers, |i| {
                calls[i].fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
                Ok(i * 10)
            })
            .unwrap();
            assert_eq!(
                results,
                (0..count).map(|i| i * 10).collect::<Vec<_>>(),
                "workers={workers}"
            );
            assert!(
                calls.iter().all(|c| c.load(Ordering::SeqCst) == 1),
                "workers={workers} count={count}"
            );
        }
    }
    let failed = run_pool(5, 3, |i| {
        if i == 3 {
            Err(anyhow!("job {i} broke"))
        } else {
            Ok(i)
        }
    });
    assert_eq!(failed.unwrap_err().to_string(), "job 3 broke");
}

#[test]
fn operand_ranges_ignore_wrapping_parentheses() {
    assert_eq!(core_range(10, "a > 0"), (10, 15));
    assert_eq!(core_range(10, "(a > 0)"), (11, 16));
    assert_eq!(core_range(10, " ( (a > 0) ) "), (14, 19));
    // Two groups, not one wrapper.
    assert_eq!(core_range(0, "(a) && (b)"), (0, 10));
    assert_eq!(core_range(0, "f(a)"), (0, 4));
    assert_eq!(core_range(3, "()"), (4, 4));
    // Text that is not balanced is left as it is.
    assert_eq!(core_range(0, "(a"), (0, 2));
    assert_eq!(core_range(0, "a)"), (0, 2));
    assert_eq!(core_range(0, ")"), (0, 1));
}

#[test]
fn pool_never_uses_more_threads_than_asked() {
    for workers in [1, 2] {
        let seen = std::sync::Mutex::new(HashSet::new());
        run_pool(6, workers, |_| {
            seen.lock().unwrap().insert(std::thread::current().id());
            // Long enough for an idle extra thread to take the next job.
            std::thread::sleep(Duration::from_millis(30));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.lock().unwrap().len(), workers);
    }
}

#[test]
fn workspace_dependents() {
    let package = |name: &str, deps: &[&str]| Package {
        name: name.to_string(),
        features: BTreeMap::new(),
        dependencies: deps
            .iter()
            .map(|d| Dependency {
                name: d.to_string(),
            })
            .collect(),
        manifest_path: PathBuf::new(),
        targets: vec![],
    };
    let packages = [
        package("core", &["serde"]),
        package("mid", &["core"]),
        package("app", &["mid", "other"]),
        package("other", &[]),
        package("cycle", &["app", "core"]),
    ];
    assert_eq!(dependents_of("core", &packages), ["app", "cycle", "mid"]);
    assert_eq!(dependents_of("mid", &packages), ["app", "cycle"]);
    assert_eq!(dependents_of("other", &packages), ["app", "cycle"]);
    assert!(dependents_of("app", &packages) == ["cycle"]);
    assert!(dependents_of("nobody", &packages).is_empty());
    // A package is not its own dependent, whatever depends on what.
    let mutual = [package("x", &["y"]), package("y", &["x"])];
    assert_eq!(dependents_of("x", &mutual), ["y"]);
}

#[test]
fn failure_lines_of_libtest() {
    assert!(is_failure_line("test tests::a ... FAILED"));
    assert!(is_failure_line(
        "test src/lib.rs - double (line 3) ... FAILED"
    ));
    assert!(!is_failure_line("test tests::a ... ok"));
    assert!(!is_failure_line("test result: FAILED. 1 passed; 1 failed"));
    assert!(!is_failure_line("my output says ... FAILED"));
    assert!(!is_failure_line(
        "test tests::a ... FAILED (but printed by the test)"
    ));
}

#[test]
fn effects_in_words() {
    let source = "fn f(s: &mut S) -> bool { s.reset(); s.count > 1 }";
    let mut found = instrument::discover(source, 0).unwrap().sites;
    found.retain(|s| !matches!(s.kind, SiteKind::Body { .. }));
    for (id, site) in found.iter_mut().enumerate() {
        site.id = id as u32;
    }
    let sites: BTreeMap<u32, (&str, &Site)> =
        found.iter().map(|s| (s.id, ("src/lib.rs", s))).collect();
    let texts: Vec<&str> = found.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        ["fn f(s: &mut S) -> bool", "s.reset();", "s.count > 1"]
    );

    let set = |list: &[Change]| list.iter().cloned().collect::<BTreeSet<Change>>();
    let mut changes = BTreeMap::new();
    changes.insert(
        "tests::a".to_string(),
        set(&[
            Change::Returns {
                site: 0,
                value: true,
            },
            Change::NowRuns { site: 1 },
        ]),
    );
    changes.insert(
        "tests::b".to_string(),
        set(&[Change::NowRuns { site: 1 }, Change::NewOutcome { site: 2 }]),
    );
    changes.insert("?".to_string(), set(&[Change::NoLongerRuns { site: 2 }]));
    assert_eq!(
        describe_effects(&changes, &sites),
        [
            "with the mutant, `tests::a` gets `true` from `fn f(s: &mut S) -> bool` (src/lib.rs:1)",
            "with the mutant, `tests::a` (and 1 more) now runs `s.reset();` (src/lib.rs:1)",
            "with the mutant, a test no longer runs `s.count > 1` (src/lib.rs:1)",
            "(and 1 more such changes)",
        ]
    );
    // What is not listed is counted.
    let mut many = BTreeMap::new();
    many.insert(
        "tests::a".to_string(),
        set(&[
            Change::NowRuns { site: 0 },
            Change::NowRuns { site: 1 },
            Change::NowRuns { site: 2 },
            Change::NoLongerRuns { site: 0 },
            Change::NoLongerRuns { site: 1 },
            Change::NoLongerRuns { site: 2 },
        ]),
    );
    assert_eq!(
        describe_effects(&many, &sites).last().map(String::as_str),
        Some("(and 3 more such changes)")
    );
    // Up to three changes are all listed, with nothing after them.
    let mut few = BTreeMap::new();
    few.insert(
        "tests::a".to_string(),
        set(&[
            Change::NowRuns { site: 1 },
            Change::NewOutcome { site: 2 },
            Change::Returns {
                site: 0,
                value: false,
            },
        ]),
    );
    assert_eq!(
        describe_effects(&few, &sites),
        [
            "with the mutant, `tests::a` gets `false` from `fn f(s: &mut S) -> bool` (src/lib.rs:1)",
            "with the mutant, `tests::a` now runs `s.reset();` (src/lib.rs:1)",
            "with the mutant, `tests::a` takes `s.count > 1` (src/lib.rs:1) a way it did not before",
        ]
    );
    assert_eq!(
        describe_effects(&BTreeMap::new(), &sites),
        [
            "with the mutant, these tests still reach the same code with the same outcomes, as far as that is recorded: they do not use its result, it is the same for their inputs, or what they assert holds for both"
        ]
    );
}

#[test]
fn mutant_timeout_scales_with_the_baseline() {
    assert_eq!(mutant_timeout(Duration::ZERO), Duration::from_secs(5));
    assert_eq!(
        mutant_timeout(Duration::from_secs(2)),
        Duration::from_secs(15)
    );
}

#[cfg(unix)]
#[test]
fn stopping_nextest_reaps_a_runner_that_ignores_termination() {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;

    let mut child = Command::new("sh")
        // Ends by itself after a minute, should a test run that is cut short leave it.
        .args([
            "-c",
            "trap '' TERM; printf X; n=0; while [ $n -lt 60 ]; do sleep 1; n=$((n+1)); done",
        ])
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdout.take().unwrap().read_exact(&mut [0]).unwrap();

    stop_nextest(&mut child, Duration::from_millis(10));
    let status = child.try_wait().unwrap().expect("runner was reaped");
    assert!(!status.success());
}

#[test]
fn nextest_counts_only_a_test_failure_as_a_kill() {
    assert_eq!(nextest_outcome(Some(0)).unwrap(), Outcome::Pass);
    assert_eq!(nextest_outcome(Some(100)).unwrap(), Outcome::Fail);
    for code in [Some(4), Some(101), Some(102), Some(105), None] {
        assert!(nextest_outcome(code).is_err(), "{code:?}");
    }
}

#[test]
fn test_polling_backs_off_and_respects_the_remaining_time() {
    let mut delay = TEST_POLL_INITIAL;
    for micros in [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000] {
        assert_eq!(
            next_test_poll(&mut delay, Duration::from_secs(1)),
            Duration::from_micros(micros)
        );
    }
    let remaining = Duration::from_micros(120);
    assert_eq!(next_test_poll(&mut delay, remaining), remaining);
    assert_eq!(next_test_poll(&mut delay, Duration::ZERO), Duration::ZERO);
    assert_eq!(delay, Duration::from_millis(5));
}
/// A scratch directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tmt-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("ws/src")).unwrap();
    fs::create_dir_all(dir.join("work/src")).unwrap();
    dir.canonicalize().unwrap()
}

#[cfg(unix)]
#[test]
fn a_descendant_cannot_keep_the_completed_runner_output_open() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch("descendant-output");
    let work = dir.join("work");
    let exe = work.join("runner");
    fs::write(&exe, "#!/bin/sh\nsleep 8 &\nexit 0\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let bin = TestBinary {
        name: "runner".into(),
        cwd: work.join("src"),
        marks: marks_dir(&work).unwrap(),
        git_ceiling: work,
        work_lock: None,
        tests: BTreeSet::new(),
        runner: Runner::Exe(exe),
        baseline: Duration::ZERO,
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::ZERO,
        timeout: None,
        wrapper_grace: Duration::ZERO,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    };
    let started = Instant::now();
    assert_eq!(
        run_binary(
            &bin,
            None,
            &[],
            Duration::from_secs(2),
            RunOutput::FirstFailure,
            1
        )
        .unwrap(),
        Outcome::Pass
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "a descendant held the output pipe open"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn a_detached_descendant_does_not_hold_the_runner_open() {
    let dir = scratch("detached-output");
    let work = dir.join("work");
    let source = work.join("runner.rs");
    let exe = work.join("runner");
    fs::write(
        &source,
        r#"
use std::os::unix::process::CommandExt;
fn main() {
    let child = std::process::Command::new("sleep").arg("8").process_group(0).spawn().unwrap();
    std::fs::write("child.pid", child.id().to_string()).unwrap();
}
"#,
    )
    .unwrap();
    let output = Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bin = TestBinary {
        name: "runner".into(),
        cwd: work.join("src"),
        marks: marks_dir(&work).unwrap(),
        git_ceiling: work,
        work_lock: None,
        tests: BTreeSet::new(),
        runner: Runner::Exe(exe),
        baseline: Duration::ZERO,
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::ZERO,
        timeout: None,
        wrapper_grace: Duration::ZERO,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    };
    let started = Instant::now();
    let outcome = run_binary(
        &bin,
        None,
        &[],
        Duration::from_secs(2),
        RunOutput::FirstFailure,
        1,
    );
    let elapsed = started.elapsed();
    // This test alone owns the separate child group.
    let pid = fs::read_to_string(bin.cwd.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    kill_test_group(pid);
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(outcome.unwrap(), Outcome::Pass);
    assert!(
        elapsed < Duration::from_secs(6),
        "a detached descendant held the output pipe open"
    );
}

fn set_modified(path: &Path, time: std::time::SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

fn modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// What a run does to the working copy up to the build. `write` stands for the
/// instrumentation of the files in `selected`.
fn copy_run(dir: &Path, selected: &[&str], write: bool) -> WorkCopy {
    let (workspace, work) = (dir.join("ws"), dir.join("work"));
    let copy = work.join("src");
    let state = WorkCopy::open(&work, &copy);
    copy_tree(&workspace, &copy, &workspace, &|_| false, &state).unwrap();
    prune(&copy, &state).unwrap();
    for name in selected {
        let path = copy.join(name);
        let original = state.original(&path, &workspace).unwrap().unwrap();
        assert_eq!(original, fs::read_to_string(workspace.join(name)).unwrap());
        if write {
            state
                .write(&path, &format!("instrumented {original}"))
                .unwrap();
        } else {
            state.record(&path).unwrap(); // interrupted before the write
        }
    }
    if write {
        state.settle(&workspace).unwrap();
    }
    state
}

/// Rows generated by `pict formal/pict/workcopy.pict /o:3 /e:formal/pict/workcopy.seed.tsv`.
#[test]
fn work_copy_table() {
    let table = include_str!("../../tests/data/workcopy.pict.tsv");
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    let mut rows = 0;
    for (index, line) in table.lines().skip(1).enumerate() {
        let [earlier, source, selected] = line.split('\t').collect::<Vec<_>>()[..] else {
            panic!("bad row: {line}");
        };
        let dir = scratch(&format!("copy-table-{index}"));
        let (file, copied) = (dir.join("ws/src/a.rs"), dir.join("work/src/src/a.rs"));
        let log = dir.join("work").join(WRITTEN_LOG);
        fs::write(&file, "v1").unwrap();
        set_modified(&file, hour_ago);
        match earlier {
            "none" => {}
            "plain" => drop(copy_run(&dir, &[], true)),
            "written" => drop(copy_run(&dir, &["src/a.rs"], true)),
            "logged_only" => drop(copy_run(&dir, &["src/a.rs"], false)),
            _ => panic!("{line}"),
        }
        if copied.exists() {
            set_modified(&copied, hour_ago);
        }
        let before = fs::read_to_string(&copied).ok();
        match source {
            "same" => {}
            "edited" => fs::write(&file, "version 2").unwrap(),
            "deleted" => fs::remove_file(&file).unwrap(),
            _ => panic!("{line}"),
        }
        let selected = selected == "yes";
        copy_run(&dir, if selected { &["src/a.rs"] } else { &[] }, true);

        let wanted = match (source, selected) {
            ("deleted", _) => None,
            ("same", true) => Some("instrumented v1".to_string()),
            ("same", false) => Some("v1".to_string()),
            (_, true) => Some("instrumented version 2".to_string()),
            (_, false) => Some("version 2".to_string()),
        };
        assert_eq!(fs::read_to_string(&copied).ok(), wanted, "{line}");
        assert_eq!(
            fs::read_to_string(&log).unwrap_or_default(),
            if selected { "src/a.rs\n" } else { "" },
            "{line}"
        );
        // What did not change was not written again, apart from the copy an
        // interrupted run left under a log entry.
        if before == wanted && wanted.is_some() && earlier != "logged_only" {
            assert_eq!(modified(&copied), hour_ago, "{line}");
        }
        // What did change looks new to cargo, although its source is an hour old:
        // a copy that kept the time of its source would not be rebuilt.
        if before != wanted && wanted.is_some() {
            assert!(
                modified(&copied) > hour_ago + Duration::from_secs(1800),
                "{line}"
            );
        }
        // A further run that instruments nothing leaves a plain copy.
        copy_run(&dir, &[], true);
        assert_eq!(
            fs::read_to_string(&copied).ok(),
            fs::read_to_string(&file).ok(),
            "{line}"
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "", "{line}");
        fs::remove_dir_all(&dir).unwrap();
        rows += 1;
    }
    assert_eq!(rows, 20);
}

#[test]
fn streaming_copy_comparison_matches_bytes() {
    use std::io::{self, Read};

    struct Chunks<'a> {
        rest: &'a [u8],
        chunk: usize,
        interrupt: bool,
        fail: bool,
    }

    impl Read for Chunks<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.fail {
                self.fail = false;
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            let limit = out.len().min(self.chunk);
            self.rest.read(&mut out[..limit])
        }
    }

    for row in include_str!("../../tests/data/workcopy-compare.pict.tsv")
        .lines()
        .skip(1)
    {
        let fields: Vec<_> = row.split('\t').collect();
        let [size, relation, reads] = fields.as_slice() else {
            panic!("{row}")
        };
        let size = match *size {
            "empty" => 0,
            "before" => 65535,
            "boundary" => 65536,
            "after" => 65537,
            "multi" => 131079,
            _ => panic!("{row}"),
        };
        let left: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut right = left.clone();
        match *relation {
            "equal" => (),
            "early" => right[0] ^= 1,
            "late" => right[size - 1] ^= 1,
            "suffix" => right.push(1),
            "shorter" => {
                right.pop();
            }
            _ => panic!("{row}"),
        }
        let make_reader = |first, rest, fail_first| Chunks {
            rest,
            chunk: if *reads == "regular" {
                usize::MAX
            } else if first {
                1031
            } else {
                2063
            },
            interrupt: *reads == "interrupted",
            fail: first == fail_first && *reads == "failed",
        };
        for (a, b, fail_first) in [
            (&left[..], &right[..], true),
            (&right[..], &left[..], false),
        ] {
            let compared = compare_streams(
                make_reader(true, a, fail_first),
                make_reader(false, b, fail_first),
            );
            if *reads == "failed" {
                assert_eq!(
                    compared.unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied,
                    "{row}"
                );
            } else {
                assert_eq!(compared.unwrap(), a == b, "{row}");
            }
        }
    }
}

#[test]
fn work_copy_keeps_modification_times() {
    let dir = scratch("copy-times");
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    let names = ["src/lib.rs", "src/plain.rs", "src/edited.rs"];
    for name in names {
        fs::write(dir.join("ws").join(name), format!("// {name}")).unwrap();
        set_modified(&dir.join("ws").join(name), hour_ago);
    }
    let copy = dir.join("work/src");
    let runtime = copy.join("src/__tmt_rt.rs");
    let run = || {
        let state = copy_run(&dir, &[], false);
        for name in ["src/lib.rs", "src/edited.rs"] {
            let original = state.original(&copy.join(name), &dir.join("ws")).unwrap();
            state
                .write(&copy.join(name), &format!("probe {}", original.unwrap()))
                .unwrap();
        }
        // A file of the tool's own, with no source: not taken for one to instrument.
        assert_eq!(state.original(&runtime, &dir.join("ws")).ok(), {
            if runtime.exists() { Some(None) } else { None }
        });
        state.write(&runtime, "runtime").unwrap();
        state.settle(&dir.join("ws")).unwrap();
    };
    run();
    for name in names.iter().map(|n| copy.join(n)).chain([runtime.clone()]) {
        set_modified(&name, hour_ago);
    }
    fs::write(dir.join("ws/src/edited.rs"), "// edited since").unwrap();
    run();
    // Nothing but the edited file was touched: cargo has one file to look at.
    for name in ["src/lib.rs", "src/plain.rs", "src/__tmt_rt.rs"] {
        assert_eq!(modified(&copy.join(name)), hour_ago, "{name}");
    }
    assert_ne!(modified(&copy.join("src/edited.rs")), hour_ago);
    assert_eq!(
        fs::read_to_string(copy.join("src/edited.rs")).unwrap(),
        "probe // edited since"
    );
    assert_eq!(
        fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "probe // src/lib.rs"
    );
    assert_eq!(
        fs::read_to_string(dir.join("work").join(WRITTEN_LOG)).unwrap(),
        "src/__tmt_rt.rs\nsrc/edited.rs\nsrc/lib.rs\n"
    );
    // A source that was only touched is still what the copy holds: nothing to do.
    let later = hour_ago + Duration::from_secs(60);
    set_modified(&dir.join("ws/src/plain.rs"), later);
    run();
    assert_eq!(modified(&copy.join("src/plain.rs")), hour_ago);
    // Other bytes are copied, also where the size and the time of the source say
    // nothing (a file put back as it was long ago).
    let plain = fs::read_to_string(dir.join("ws/src/plain.rs")).unwrap();
    fs::write(dir.join("ws/src/plain.rs"), plain.to_uppercase()).unwrap();
    set_modified(
        &dir.join("ws/src/plain.rs"),
        hour_ago - Duration::from_secs(60),
    );
    run();
    assert_eq!(
        fs::read_to_string(copy.join("src/plain.rs")).unwrap(),
        plain.to_uppercase()
    );
    assert!(modified(&copy.join("src/plain.rs")) > later);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn work_copy_log_is_not_trusted() {
    let dir = scratch("copy-log");
    let (workspace, work) = (dir.join("ws"), dir.join("work"));
    let copy = work.join("src");
    let outside = dir.join("outside.txt");
    fs::write(&outside, "keep").unwrap();
    fs::write(workspace.join("src/a.rs"), "a").unwrap();
    // Entries that leave the copy are not entries.
    fs::write(
        work.join(WRITTEN_LOG),
        format!(
            "../../outside.txt\n{}\n\nsrc/../../../outside.txt\nsrc/a.rs\n",
            outside.display()
        ),
    )
    .unwrap();
    let state = WorkCopy::open(&work, &copy);
    assert_eq!(
        state.earlier,
        [copy.join("src/a.rs")].into_iter().collect::<HashSet<_>>()
    );
    copy_tree(&workspace, &copy, &workspace, &|_| false, &state).unwrap();
    state.settle(&workspace).unwrap();
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    assert_eq!(fs::read_to_string(copy.join("src/a.rs")).unwrap(), "a");

    // Nothing is written outside the copy, and a name that would break the log is refused.
    let state = WorkCopy::open(&work, &copy);
    assert!(state.write(&outside, "x").is_err());
    assert!(state.write(&copy.join("src/a\nb.rs"), "x").is_err());
    assert!(!copy.join("src/a\nb.rs").exists());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    fs::remove_dir_all(&dir).unwrap();
}
#[test]
fn work_copy_log_follows_every_write() {
    let dir = scratch("copy-log-order");
    let (workspace, work) = (dir.join("ws"), dir.join("work"));
    let copy = work.join("src");
    let log = work.join(WRITTEN_LOG);
    let names = ["h", "c", "f", "a", "e", "b", "g", "d"].map(|n| format!("src/{n}.rs"));
    for name in &names {
        fs::write(workspace.join(name), "x").unwrap();
    }
    let state = copy_run(&dir, &[], false);
    // The entry is there before the file changes, once.
    state.record(&copy.join(&names[0])).unwrap();
    state.record(&copy.join(&names[0])).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "src/h.rs\n");
    assert_eq!(fs::read_to_string(copy.join(&names[0])).unwrap(), "x");
    for name in &names {
        state.write(&copy.join(name), "probe").unwrap();
    }
    state.settle(&workspace).unwrap();
    let mut sorted = names.to_vec();
    sorted.sort();
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        sorted.iter().map(|n| format!("{n}\n")).collect::<String>()
    );

    // After the log was trimmed, a later write is logged again.
    let state = copy_run(&dir, &[], false);
    state.settle(&workspace).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "");
    state.write(&copy.join(&names[3]), "probe").unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "src/a.rs\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sync_copies_what_is_not_up_to_date_and_nothing_else() {
    let dir = scratch("copy-sync");
    let (workspace, work) = (dir.join("ws"), dir.join("work"));
    let copy = work.join("src");
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    for name in [
        "same",
        "longer",
        "newer",
        "was_dir",
        "was_link",
        "short_link",
        "twin_link",
        "shorter",
    ] {
        fs::write(workspace.join("src").join(name), "source").unwrap();
        set_modified(&workspace.join("src").join(name), hour_ago);
    }
    // A file that cannot be written to is copied like any other.
    let locked = workspace.join("src/locked");
    fs::write(&locked, "read only").unwrap();
    let mut permissions = fs::metadata(&locked).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&locked, permissions).unwrap();
    fs::create_dir_all(copy.join("src/was_dir/inner")).unwrap();
    // The same size and newer, and not the same: no copy of the source.
    fs::write(copy.join("src/same"), "SOURCE").unwrap();
    // A copy is one whatever its time says, and keeps that time (nothing to rebuild).
    fs::write(workspace.join("src/copied"), "as it is").unwrap();
    fs::write(copy.join("src/copied"), "as it is").unwrap();
    let long_ago = hour_ago - Duration::from_secs(7200);
    set_modified(&copy.join("src/copied"), long_ago);
    // A directory of the copy that has become a link is not walked into.
    let elsewhere = dir.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, copy.join("linked")).unwrap();
    fs::create_dir_all(workspace.join("linked/deep")).unwrap();
    fs::write(workspace.join("linked/deep/z"), "z").unwrap();
    fs::write(copy.join("src/longer"), "SOURCE+").unwrap();
    fs::write(copy.join("src/newer"), "SOURCE").unwrap();
    set_modified(&copy.join("src/newer"), hour_ago - Duration::from_secs(60));
    // A link in the copy is replaced, not written through.
    let outside = dir.join("outside");
    fs::write(&outside, "keep").unwrap();
    std::os::unix::fs::symlink(&outside, copy.join("src/was_link")).unwrap();
    // Not even one to a file that holds what the source holds.
    let twin = dir.join("twin");
    fs::write(&twin, "source").unwrap();
    std::os::unix::fs::symlink(&twin, copy.join("src/twin_link")).unwrap();
    // The beginning of the source is not the source.
    fs::write(copy.join("src/shorter"), "sour").unwrap();
    // Not even one whose own size and time would pass for the file's.
    std::os::unix::fs::symlink("abcdef", copy.join("src/short_link")).unwrap();
    // What the run does not want goes, with the directories it leaves empty.
    fs::create_dir_all(copy.join("old/deep")).unwrap();
    fs::write(copy.join("old/deep/x"), "x").unwrap();
    fs::create_dir_all(workspace.join("keep/deep")).unwrap();
    fs::write(workspace.join("keep/deep/y"), "y").unwrap();

    copy_run(&dir, &[], true);
    let read = |name: &str| fs::read_to_string(copy.join(name)).unwrap();
    assert_eq!(read("src/locked"), "read only");
    assert_eq!(read("src/copied"), "as it is");
    assert_eq!(
        fs::metadata(copy.join("src/copied"))
            .unwrap()
            .modified()
            .unwrap(),
        long_ago
    );
    assert_eq!(read("linked/deep/z"), "z");
    assert!(fs::symlink_metadata(copy.join("linked")).unwrap().is_dir());
    assert_eq!(
        fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing written through"
    );
    for name in [
        "src/same",
        "src/longer",
        "src/newer",
        "src/was_dir",
        "src/was_link",
        "src/short_link",
        "src/twin_link",
        "src/shorter",
    ] {
        assert_eq!(read(name), "source", "{name}");
        assert!(
            fs::symlink_metadata(copy.join(name)).unwrap().is_file(),
            "{name}"
        );
    }
    assert!(
        fs::symlink_metadata(copy.join("src/was_link"))
            .unwrap()
            .is_file()
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    assert!(!copy.join("old").exists());
    assert_eq!(read("keep/deep/y"), "y");
    fs::remove_dir_all(&dir).unwrap();
}
#[test]
fn a_finding_keeps_its_number_when_the_one_before_it_is_closed() {
    let source = "impl S {\n    fn f(a: u8, b: u8) -> u8 {\n        let x = if a > b { 1 } else { 0 };\n        let y = if a > b { 2 } else { 0 };\n        x + y\n    }\n}\nfn g(a: u8, b: u8) -> bool {\n    a > b\n}\n";
    let found = instrument::discover(source, 0).unwrap();
    let mut all: Vec<(usize, usize, String)> = found
        .sites
        .iter()
        .map(|site| {
            let at = report::location("", site);
            (at.line, at.column, at.code)
        })
        .collect();
    all.push(
        all.iter()
            .find(|(line, _, code)| *line == 3 && code == "a > b")
            .unwrap()
            .clone(),
    );
    all.reverse();
    let places = [FilePlaces {
        display: "src/lib.rs",
        source,
        lines: &scope::LineIndex::new(source),
        places: &all,
    }];
    let gap = |line: usize, column: usize, file: &str| Finding::InputGap {
        at: report::Location {
            file: file.to_string(),
            line,
            column,
            code: "a > b".to_string(),
            ..report::Location::default()
        },
        survivors: vec![],
        survivor_count: 0,
        add_tests: vec![],
        observed: None,
        verified: true,
    };
    let name = |finding: &Finding| {
        let at = finding.at();
        (at.item.clone(), at.id.clone())
    };
    // Only the second of the two in `f` is a finding: it is still the second.
    let mut findings = [
        gap(4, 20, "src/lib.rs"),
        gap(9, 5, "src/lib.rs"),
        gap(9, 5, "src/other.rs"),
        gap(5, 9, "src/lib.rs"), // A finding without a corresponding catalogued location.
    ];
    name_findings(&mut findings, &places);
    assert_eq!(
        name(&findings[0]),
        (
            "S::f".to_string(),
            "input_gap|src/lib.rs|S::f|a > b|1".to_string()
        )
    );
    assert_eq!(
        name(&findings[1]),
        (
            "g".to_string(),
            "input_gap|src/lib.rs|g|a > b|0".to_string()
        )
    );
    // A finding of a file that is not among the given ones is left without a name.
    assert_eq!(name(&findings[2]), (String::new(), String::new()));
    assert_eq!(
        name(&findings[3]),
        (
            "S::f".to_string(),
            "input_gap|src/lib.rs|S::f|a > b|2".to_string()
        )
    );
}

#[test]
fn an_audit_searches_only_the_jobs_for_its_site() {
    let jobs: Vec<_> = [1, 1, 3, 8, 8]
        .into_iter()
        .map(|site| Job {
            site,
            mutation: Mutation::Negate,
            tests: vec![],
        })
        .collect();
    for (site, range) in [
        (0, 0..0),
        (1, 0..2),
        (2, 2..2),
        (3, 2..3),
        (4, 3..3),
        (8, 3..5),
        (9, 5..5),
    ] {
        let found = jobs_for_site(&jobs, site);
        assert_eq!(found, range);
        assert_eq!(
            found
                .clone()
                .find(|&index| jobs[index].mutation == Mutation::Negate),
            jobs.iter()
                .position(|job| job.site == site && job.mutation == Mutation::Negate)
        );
    }
}

#[test]
fn an_audit_takes_its_sample_evenly_and_the_same_every_time() {
    assert_eq!(spread(0, 3), Vec::<usize>::new());
    assert_eq!(spread(5, 0), Vec::<usize>::new());
    assert_eq!(spread(3, 3), [0, 1, 2]);
    assert_eq!(spread(3, 10), [0, 1, 2]);
    assert_eq!(spread(10, 1), [0]);
    assert_eq!(spread(10, 3), [0, 3, 6]);
    assert_eq!(spread(10, 5), [0, 2, 4, 6, 8]);
    assert_eq!(spread(4, 3), [0, 1, 2]);
    assert_eq!(spread(7, 6), [0, 1, 2, 3, 4, 5]);
}

#[test]
fn a_mutant_of_the_source_is_the_mutation_its_probe_runs() {
    // The runtime and the driver have to mean the same status.
    // The runtime ends a run it gives up as a failed one: the mark says which it was.
    assert_ne!(crate::tmt_rt::CANNOT_EVALUATE, 0);
    let source = "fn f(a: i32, b: i32, ok: bool, k: Option<i32>, base: O) -> O { \
                      go(); \
                      let x = match k { Some(n) if n > 1 => !ok && a < b, _ => -a + b > 0 || ok }; \
                      for _ in a..b {} for _ in a..=b {} \
                      O { on: x, ..base } }";
    let found = instrument::discover(source, 0).unwrap().sites;
    let mut seen = BTreeSet::new();
    for site in &found {
        for mutant in instrument::source_mutants(site) {
            let kind = format!("{:?}", site.kind);
            let kind = kind.split([' ', '{', '(']).next().unwrap().to_string();
            seen.insert((
                kind,
                mutant.into.clone(),
                format!("{:?}", mutation_of(site, &mutant)),
            ));
        }
    }
    let expected = [
        ("Arith", "*", "ArithSwap2"),
        ("Arith", "-", "ArithSwap"),
        ("Arm", "Some(n) if n > 1", "Delete"),
        ("Body", "Default::default()", "Delete"),
        ("Cmp", "<", "Op(Lt)"),
        ("Cmp", "<=", "Op(Le)"),
        ("Cmp", "==", "Op(Eq)"),
        ("Cmp", ">", "Op(Gt)"),
        ("Cmp", ">=", "Op(Ge)"),
        ("Field", "on", "Delete"),
        ("Guard", "true", "Const(true)"),
        ("Logic", "&&", "LogicSwap"),
        ("Logic", "||", "LogicSwap"),
        ("Neg", "", "Delete"),
        ("Not", "", "DropNot"),
        ("Range", "..", "RangeEnd"),
        ("Range", "..=", "RangeEnd"),
        ("Stmt", "", "Delete"),
    ];
    // A comparison is planned in other words than it is written in: the mutations
    // that are the same change, as an audit looks for them.
    let like = |source: &str, into: &str| -> Vec<Mutation> {
        let sites = instrument::discover(source, 0).unwrap().sites;
        let site = sites
            .iter()
            .find(|site| matches!(site.kind, SiteKind::Cmp { .. }))
            .unwrap();
        let mutant = instrument::source_mutants(site)
            .into_iter()
            .find(|mutant| mutant.into == into)
            .unwrap();
        mutations_like(site, &mutant, true)
    };
    use plan::CmpOp::*;
    for (op, into, expected) in [
        ("==", "!=", vec![Mutation::Op(Ne), Mutation::Negate]),
        ("!=", "==", vec![Mutation::Op(Eq), Mutation::Negate]),
        ("<=", ">", vec![Mutation::Op(Gt), Mutation::Negate]),
        (">=", "<", vec![Mutation::Op(Lt), Mutation::Negate]),
        ("<", "<=", vec![Mutation::Op(Le), Mutation::Shift(1)]),
        (">", ">=", vec![Mutation::Op(Ge), Mutation::Shift(-1)]),
        ("<", "==", vec![Mutation::Op(Eq)]),
        ("<", ">", vec![Mutation::Op(Gt)]),
        (">", "==", vec![Mutation::Op(Eq)]),
        (">", "<", vec![Mutation::Op(Lt)]),
    ] {
        let source = format!("fn f(a: i32, b: i32) -> bool {{ a {op} b }}");
        assert_eq!(like(&source, into), expected, "{op} as {into}");
    }
    // Where the tests gave operands without an order, the negation of `>=` is not
    // `<`: only the operator itself is the same change. `!=` is still the negation.
    let unordered = |op: &str, into: &str| -> Vec<Mutation> {
        let source = format!("fn f(a: f64, b: f64) -> bool {{ a {op} b }}");
        let sites = instrument::discover(&source, 0).unwrap().sites;
        let site = sites
            .iter()
            .find(|site| matches!(site.kind, SiteKind::Cmp { .. }))
            .unwrap();
        let mutant = instrument::source_mutants(site)
            .into_iter()
            .find(|mutant| mutant.into == into)
            .unwrap();
        mutations_like(site, &mutant, false)
    };
    assert_eq!(unordered(">=", "<"), [Mutation::Op(Lt)]);
    assert_eq!(unordered("<=", ">"), [Mutation::Op(Gt)]);
    assert_eq!(unordered("==", "!="), [Mutation::Op(Ne), Mutation::Negate]);
    // The moved bound is the operator it is taken for, at every distance.
    for d in -3..=3 {
        assert_eq!(
            plan::eval_mutated_d(Lt, Mutation::Shift(1), d),
            Le.eval_d(d)
        );
        assert_eq!(
            plan::eval_mutated_d(Gt, Mutation::Shift(-1), d),
            Ge.eval_d(d)
        );
    }
    let seen: Vec<(&str, &str, &str)> = seen
        .iter()
        .map(|(kind, into, mutation)| (kind.as_str(), into.as_str(), mutation.as_str()))
        .collect();
    assert_eq!(seen, expected);
}

#[test]
fn the_mark_of_a_run_is_a_new_file_and_says_one_of_three_things() {
    let dir = scratch("unable-mark");
    fs::create_dir_all(&dir).unwrap();
    let runs = std::sync::atomic::AtomicUsize::new(0);
    let name = |n: usize| dir.join(format!("tmt-unable-{}-{n}", std::process::id()));
    // A file of the name that would be next belongs to someone: it is not emptied,
    // and the mark gets another name.
    fs::write(name(0), "not ours").unwrap();
    fs::write(name(1), "nor this").unwrap();
    let mark = reserve_file(&dir, "unable", &runs).unwrap();
    assert_eq!(mark, name(2));
    assert!(mark.is_absolute());
    assert_eq!(fs::read_to_string(&mark).unwrap(), "");
    assert_eq!(fs::read_to_string(name(0)).unwrap(), "not ours");
    assert_eq!(fs::read_to_string(name(1)).unwrap(), "nor this");
    // Still there after the run: the run went to its end, and the mark is taken away.
    assert!(!release_mark(&mark).unwrap());
    assert!(!mark.exists());
    // Gone: the run gave up.
    let mark = reserve_file(&dir, "unable", &runs).unwrap();
    assert_eq!(mark, name(3));
    fs::remove_file(&mark).unwrap();
    assert!(release_mark(&mark).unwrap());
    // There, and not to be taken away (a directory is not a file to remove): neither
    // of the two, and said as an error. Whoever runs the tests, with whatever rights.
    let in_the_way = dir.join("a-directory");
    fs::create_dir_all(&in_the_way).unwrap();
    let refused = format!("{:#}", release_mark(&in_the_way).unwrap_err());
    assert!(refused.contains("cannot tell whether"), "{refused}");
    assert!(refused.contains("a-directory"), "{refused}");
    assert!(in_the_way.is_dir());
    // A mark cannot be made in what is no directory, and that is not tried for ever.
    let unmade = format!("{:#}", reserve_file(&name(0), "unable", &runs).unwrap_err());
    assert!(unmade.contains("creating"), "{unmade}");
    assert_eq!(fs::read_to_string(name(0)).unwrap(), "not ours");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_directory_of_the_marks_is_made_or_used_and_nothing_is_removed_for_it() {
    let dir = scratch("marks-dir");
    let work = dir.join("work");
    // Not there: made. There as a directory, with whatever is in it: used.
    assert_eq!(marks_dir(&work).unwrap(), work.join("marks"));
    assert!(work.join("marks").is_dir());
    fs::write(work.join("marks/left-over"), "x").unwrap();
    assert_eq!(marks_dir(&work).unwrap(), work.join("marks"));
    assert_eq!(
        fs::read_to_string(work.join("marks/left-over")).unwrap(),
        "x"
    );
    // A file of that name is somebody's: it stays, and the run does not start.
    let taken = dir.join("taken");
    fs::create_dir_all(&taken).unwrap();
    fs::write(taken.join("marks"), "grades of the class").unwrap();
    let said = format!("{:#}", marks_dir(&taken).unwrap_err());
    assert!(
        said.contains("is in the way") && said.contains("marks"),
        "{said}"
    );
    assert_eq!(
        fs::read_to_string(taken.join("marks")).unwrap(),
        "grades of the class"
    );
    // So is a link, whatever it points to: neither followed nor removed.
    #[cfg(unix)]
    {
        let linked = dir.join("linked");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&linked).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, linked.join("marks")).unwrap();
        assert!(marks_dir(&linked).is_err());
        assert!(
            fs::symlink_metadata(linked.join("marks"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_the_runs_left_is_said_only_when_there_was_something() {
    assert_eq!(leftover_note(0, 0), None);
    assert_eq!(
        leftover_note(3, 0).unwrap(),
        "stopped 3 processes that the tests had left running after their run"
    );
    assert_eq!(
        leftover_note(0, 2).unwrap(),
        "after 2 runs something the tests started may still be running (it could not be \
             found or stopped)"
    );
    let both = leftover_note(1, 1).unwrap();
    assert!(both.starts_with("stopped 1 processes") && both.contains("; after 1 runs"));
}

#[test]
fn a_predicted_survivor_is_unstable_only_when_a_re_run_was_found_out() {
    let run = |survived: bool, unable: bool, unconfirmed_timeout: bool| JobResult {
        first_pass_timeout: unconfirmed_timeout,
        unconfirmed_timeout,
        survived: survived.then(Vec::new),
        timed_out: unconfirmed_timeout,
        unable,
    };
    let passed = || run(true, false, false);
    let found_out = || run(false, false, false);
    let unable = || run(false, true, false);
    let unconfirmed = || run(false, false, true);
    let of = |runs: Vec<JobResult>| Prediction::of(runs.iter());
    assert_eq!(of(vec![]), Prediction::Confirmed);
    assert_eq!(of(vec![passed(), passed()]), Prediction::Confirmed);
    assert_eq!(of(vec![passed(), found_out()]), Prediction::Refuted);
    assert_eq!(of(vec![found_out(), passed()]), Prediction::Refuted);
    // A run that says nothing does not hide one that was found out, in either order.
    assert_eq!(of(vec![unable(), found_out()]), Prediction::Refuted);
    assert_eq!(of(vec![found_out(), unconfirmed()]), Prediction::Refuted);
    assert_eq!(of(vec![passed(), unable()]), Prediction::Unknown);
    assert_eq!(of(vec![unable(), passed()]), Prediction::Unknown);
    assert_eq!(of(vec![unconfirmed()]), Prediction::Unknown);
}

#[cfg(unix)]
#[test]
fn a_build_that_failed_is_not_code_the_compiler_refused() {
    use std::os::unix::process::ExitStatusExt;
    let output = |status: i32, stdout: &str, stderr: &str| std::process::Output {
        status: std::process::ExitStatus::from_raw(status),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    };
    let refusal = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","rendered":"error: mismatched types","spans":[]}}"#;
    let warning = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused","rendered":"warning: unused","spans":[]}}"#;
    // A wait status: exit code 101 is 101 << 8, a signal is its number.
    assert_eq!(source_build(&output(0, warning, "")), SourceBuild::Built);
    assert_eq!(
        source_build(&output(101 << 8, refusal, "error: could not compile")),
        SourceBuild::Refused
    );
    // Ended badly with nothing said against the code: killed, or cargo itself failed.
    let killed = source_build(&output(9, warning, "Compiling x\nlast words"));
    assert!(
        matches!(&killed, SourceBuild::Failed(why) if why.contains("last words")),
        "{killed:?}"
    );
    let cargo = source_build(&output(
        101 << 8,
        "",
        "error: failed to open lock\nno space left on device",
    ));
    assert!(
        matches!(&cargo, SourceBuild::Failed(why) if why.contains("no space left")),
        "{cargo:?}"
    );
}

#[test]
fn concurrent_runs_share_the_cores() {
    // Alone a binary has them all; four at a time have a quarter each.
    assert_eq!(test_threads(16, 1), 16);
    assert_eq!(test_threads(16, 4), 4);
    assert_eq!(test_threads(18, 4), 4);
    // Never fewer than two: a single thread is the main thread, without a name.
    assert_eq!(test_threads(16, 8), 2);
    assert_eq!(test_threads(16, 9), 2);
    assert_eq!(test_threads(4, 16), 2);
    assert_eq!(test_threads(1, 1), 2);
    assert_eq!(test_threads(8, 0), 8);
}

#[test]
fn cargo_is_told_the_same_everywhere() {
    assert!(CargoConfig::default().flags().is_empty());
    let some = CargoConfig {
        features: vec!["a".to_string(), "dep/b".to_string()],
        no_default_features: true,
        ..CargoConfig::default()
    };
    assert_eq!(
        some.flags(),
        ["--features", "a,dep/b", "--no-default-features"]
    );
    let all = CargoConfig {
        all_features: true,
        locked: true,
        ..CargoConfig::default()
    };
    assert_eq!(all.flags(), ["--all-features", "--locked"]);
}

#[test]
fn source_mutants_are_described_like_the_others() {
    let source = "fn f(a: i32, b: i32, k: Option<i32>) -> bool { \
                      match k { Some(n) if n > 1 => !(a < b) || a + b == 0, _ => -a * b > 1 } }";
    let found = instrument::discover(source, 0).unwrap().sites;
    let described: Vec<String> = found
        .iter()
        .flat_map(|site| {
            instrument::source_mutants(site)
                .into_iter()
                .map(|mutant| report::describe(site, mutation_of(site, &mutant)))
        })
        .collect();
    assert_eq!(
        described,
        [
            "the body of `f` replaced by `true`",
            "the body of `f` replaced by `false`",
            // The arm can be removed, which is its guard being `false`.
            "remove the arm `Some(n) if n > 1 => ..`",
            "the guard `n > 1` replaced by `true`",
            "`n == 1`",
            "`n < 1`",
            "`n >= 1`",
            "`!(a < b) && a + b == 0`",
            "`(a < b)`",
            "`a == b`",
            "`a > b`",
            "`a <= b`",
            "`a + b != 0`",
            "`a - b`",
            "`a * b`",
            "`-a * b == 1`",
            "`-a * b < 1`",
            "`-a * b >= 1`",
            "`-a / b`",
            "`-a + b`",
            "`a`",
        ]
    );
    // Without a catch-all arm the guard can be `false` in its own right.
    let source = "fn f(k: K) -> u8 { match k { K::A(n) if n > 1 => 1, K::A(_) => 2 } }";
    let found = instrument::discover(source, 0).unwrap().sites;
    let guard = found
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Guard { .. }))
        .unwrap();
    let described: Vec<String> = instrument::source_mutants(guard)
        .iter()
        .map(|mutant| report::describe(guard, mutation_of(guard, mutant)))
        .collect();
    assert_eq!(
        described,
        [
            "the guard `n > 1` replaced by `true`",
            "the guard `n > 1` replaced by `false`"
        ]
    );
}

#[test]
fn dependents_are_found_once_through_a_cycle_that_leaves_the_package_out() {
    let package = |name: &str, deps: &[&str]| Package {
        name: name.to_string(),
        features: BTreeMap::new(),
        dependencies: deps
            .iter()
            .map(|d| Dependency {
                name: d.to_string(),
            })
            .collect(),
        manifest_path: PathBuf::new(),
        targets: vec![],
    };
    // `x` and `y` depend on each other: the search has to end although each of them
    // is reached again from the other.
    let packages = [
        package("z", &[]),
        package("x", &["y", "z"]),
        package("y", &["x"]),
    ];
    assert_eq!(dependents_of("z", &packages), ["x", "y"]);
}

#[test]
fn an_error_is_said_once_and_only_when_it_says_something() {
    let stdout = r#"
{"reason":"compiler-message","message":{"level":"error","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"","rendered":null,"spans":[]}}
{"reason":"compiler-message","message":{"level":"error","rendered":"aborting","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","rendered":"aborting","spans":[]}}
"#;
    assert_eq!(parse_build_output(stdout, &["p"]).diagnostics, ["aborting"]);
}

/// A test binary in `dir` whose tests are the shell script `body`.
#[cfg(unix)]
fn script_binary(dir: &Path, body: &str) -> TestBinary {
    use std::os::unix::fs::PermissionsExt as _;
    let work = dir.join("work");
    let exe = work.join("runner");
    fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    TestBinary {
        name: "runner".into(),
        cwd: work.join("src"),
        marks: marks_dir(&work).unwrap(),
        git_ceiling: work,
        work_lock: None,
        tests: BTreeSet::new(),
        runner: Runner::Exe(exe),
        baseline: Duration::ZERO,
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::ZERO,
        timeout: None,
        wrapper_grace: Duration::ZERO,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    }
}

/// The same binary, run one test to a process.
#[cfg(unix)]
fn isolated(mut bin: TestBinary) -> TestBinary {
    let exe = bin.runner.executable().unwrap().to_path_buf();
    bin.runner = Runner::Isolated {
        exe,
        test: "t".to_string(),
        ignored: false,
        wrapper: None,
    };
    bin
}

/// Whether the process `pid` ends within a generous time. One that was stopped is
/// gone or, until its parent takes notice, only waits to be collected.
#[cfg(unix)]
fn ends(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&state.stdout);
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            return true;
        }
        if Instant::now() >= deadline {
            // Not left behind by a test that fails.
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether, within a generous time, no process is left that was started with `path`
/// among its arguments.
#[cfg(unix)]
fn nothing_runs(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let processes = Command::new("ps")
            .args(["-e", "-o", "args="])
            .output()
            .unwrap();
        let processes = String::from_utf8_lossy(&processes.stdout).into_owned();
        if !processes.contains(&*path.to_string_lossy()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[test]
fn a_filter_is_timed_only_for_a_slow_libtest_binary_without_a_given_limit() {
    let dir = scratch("unmeasured-filters");
    let slow = Duration::from_secs(70);
    let fast = ["fast".to_string()];
    let all = ["fast".to_string(), "slow".to_string()];
    let binary = |baseline, timeout| {
        let mut bin = script_binary(&dir, "exit 0");
        bin.tests = all.iter().cloned().collect();
        bin.baseline = baseline;
        bin.timeout = timeout;
        bin
    };
    let timed = |bin: &TestBinary| bin.filtered_timings.lock().unwrap().len();

    // A limit that was given is the limit: nothing is run to find one.
    let given = binary(slow, Some(Duration::from_secs(8)));
    assert_eq!(
        given.filtered_limit(Some(&fast), false, 1),
        Duration::from_secs(8)
    );
    assert_eq!(timed(&given), 0);
    // Tests that are fast as a whole are not timed again in parts.
    let quick = binary(Duration::from_secs(1), None);
    assert_eq!(
        quick.filtered_limit(Some(&fast), false, 1),
        Duration::from_secs(10)
    );
    assert_eq!(timed(&quick), 0);
    // From two seconds on they are.
    let at_the_threshold = binary(SLOW_TRACE, None);
    at_the_threshold.filtered_limit(Some(&fast), false, 1);
    assert_eq!(timed(&at_the_threshold), 1);
    // A runner that is not the libtest binary itself cannot be asked for a part.
    let one_by_one = isolated(binary(slow, None));
    assert_eq!(
        one_by_one.filtered_limit(Some(&fast), false, 1),
        Duration::from_secs(355)
    );
    assert_eq!(timed(&one_by_one), 0);
    // No names and all the names are the whole binary, which has its timing.
    let whole = binary(slow, None);
    for filter in [&[][..], &all[..]] {
        assert_eq!(
            whole.filtered_limit(Some(filter), false, 1),
            Duration::from_secs(355),
            "{filter:?}"
        );
    }
    assert_eq!(timed(&whole), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_command_in_the_copy_cannot_reach_the_repository_around_it() {
    use std::ffi::OsStr;
    let mut command = Command::new("git");
    isolate_copy_command(&mut command, Path::new("/work"));
    let set: Vec<_> = command.get_envs().collect();
    for removed in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        assert!(set.contains(&(OsStr::new(removed), None)), "{removed}");
    }
    assert!(set.contains(&(
        OsStr::new("GIT_CEILING_DIRECTORIES"),
        Some(OsStr::new("/work"))
    )));
}

#[cfg(unix)]
#[test]
fn the_tests_are_given_the_mark_the_mutant_and_the_fence_for_git() {
    let dir = scratch("run-environment");
    let work = dir.join("work");
    let bin = script_binary(
        &dir,
        &format!(
            "[ -f \"$TMT_UNABLE\" ] && [ \"$TMT_MUTANT\" = 7 ] \
                 && [ \"$GIT_CEILING_DIRECTORIES\" = '{}' ]",
            work.display()
        ),
    );
    assert_eq!(
        run_binary(
            &bin,
            None,
            &[("TMT_MUTANT", "7")],
            Duration::from_secs(60),
            RunOutput::Quiet,
            1
        )
        .unwrap(),
        Outcome::Pass
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn a_failure_line_ends_the_run_before_the_tests_do() {
    let dir = scratch("first-failure");
    // The tests go on for longer than the run is given: only the line can end it as
    // a failure. They and what they started are stopped then.
    let bin = script_binary(
        &dir,
        "sleep 30 &\necho $! > child.pid\necho 'test tests::a ... FAILED'\nwait",
    );
    let outcome = run_binary(
        &bin,
        None,
        &[],
        Duration::from_secs(20),
        RunOutput::FirstFailure,
        1,
    );
    let pid = fs::read_to_string(bin.cwd.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stopped = ends(pid);
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(outcome.unwrap(), Outcome::Fail);
    assert!(stopped, "the failed run left a process behind");
}

#[cfg(unix)]
#[test]
fn failure_lines_are_found_in_output_that_comes_in_pieces() {
    use std::io::Write as _;
    let dir = scratch("failure-output");
    let path = dir.join("output");
    let mut written = fs::File::create(&path).unwrap();
    let mut output = FailureOutput::new(fs::File::open(&path).unwrap());
    let mut next = |text: &str| {
        written.write_all(text.as_bytes()).unwrap();
        output.failed().unwrap()
    };
    assert!(!next(""));
    assert!(!next("running 2 tests\ntest tests::a ... ok\n"));
    // A line counts when its end has come.
    assert!(!next("test tests::b ... FA"));
    assert!(next("ILED\n"));
    // A line longer than is kept is not judged by its end, and the one after it is
    // a line of its own.
    let long = format!("test {} ... FAILED\n", "x".repeat(8 * 1024));
    assert!(!next(&long));
    assert!(!next("test tests::c ... ok\n"));
    assert!(next("test tests::d ... FAILED\n"));
    // Every line of a piece is looked at, not only its last.
    assert!(next(
        "noise\ntest tests::e ... FAILED\ntest tests::f ... ok\n"
    ));

    // The longest line that is kept, and one byte more in two pieces.
    let longest = format!("test {} ... FAILED\n", "x".repeat(8 * 1024 - 17));
    assert_eq!(longest.len(), 8 * 1024);
    for (pieces, found) in [(vec![&longest[..]], true), (vec!["t", &longest[1..]], true)] {
        let path = dir.join("longest");
        let mut written = fs::File::create(&path).unwrap();
        let mut output = FailureOutput::new(fs::File::open(&path).unwrap());
        let mut last = false;
        for piece in pieces {
            written.write_all(piece.as_bytes()).unwrap();
            last = output.failed().unwrap();
        }
        assert_eq!(last, found);
    }
    let over = format!("test {} ... FAILED\n", "x".repeat(8 * 1024 - 16));
    assert_eq!(over.len(), 8 * 1024 + 1);
    let path = dir.join("over");
    let mut written = fs::File::create(&path).unwrap();
    let mut output = FailureOutput::new(fs::File::open(&path).unwrap());
    written.write_all(b"t").unwrap();
    assert!(!output.failed().unwrap());
    written.write_all(&over.as_bytes()[1..]).unwrap();
    assert!(!output.failed().unwrap());
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn output_nobody_reads_is_discarded() {
    let dir = scratch("discarded-output");
    let nowhere = "[ /dev/stdout -ef /dev/null ] && [ /dev/stderr -ef /dev/null ]";
    let run = |bin: &TestBinary, output| {
        run_binary(bin, None, &[], Duration::from_secs(60), output, 1).unwrap()
    };
    // A quiet run is not watched at all.
    let quiet = script_binary(&dir, nowhere);
    assert_eq!(run(&quiet, RunOutput::Quiet), Outcome::Pass);
    // A test in a process of its own is judged by how the process ends, also when
    // the first failure is asked for.
    // It is told which test it is, as libtest and the runtime want to hear it.
    let one_by_one = isolated(script_binary(
        &dir,
        &format!("{nowhere} && [ \"$*\" = '--exact --test-threads=1 t' ] && [ \"$TMT_TEST\" = t ]"),
    ));
    assert_eq!(run(&one_by_one, RunOutput::FirstFailure), Outcome::Pass);
    // Only the standard output of a watched run is kept.
    let watched = script_binary(
        &dir,
        "[ ! /dev/stdout -ef /dev/null ] && [ /dev/stderr -ef /dev/null ]",
    );
    assert_eq!(run(&watched, RunOutput::FirstFailure), Outcome::Pass);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn what_a_finished_run_left_running_is_stopped() {
    let left_running = "sleep 30 &\necho $! > child.pid";
    let check = |name: &str, one_by_one: bool, output| {
        let dir = scratch(name);
        let mut bin = script_binary(&dir, left_running);
        if one_by_one {
            bin = isolated(bin);
        }
        let outcome = run_binary(&bin, None, &[], Duration::from_secs(60), output, 1);
        let pid = fs::read_to_string(bin.cwd.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let stopped = ends(pid);
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(outcome.unwrap(), Outcome::Pass, "{name}");
        assert!(stopped, "{name}: the run left a process behind");
    };
    check("left-running-watched", false, RunOutput::FirstFailure);
    check("left-running-isolated", true, RunOutput::Quiet);
}

#[cfg(unix)]
#[test]
fn tests_that_do_not_end_in_time_are_stopped_as_a_timeout() {
    let dir = scratch("run-timeout");
    let bin = script_binary(&dir, "sleep 20");
    let outcome = run_binary(
        &bin,
        None,
        &[],
        Duration::from_millis(100),
        RunOutput::Quiet,
        1,
    );
    // The shell that ran the script is known by the path of the script.
    let stopped = nothing_runs(bin.runner.executable().unwrap());
    fs::remove_dir_all(dir).unwrap();
    assert_eq!(outcome.unwrap(), Outcome::Timeout);
    assert!(stopped, "the tests went on after the timeout");
}

#[cfg(unix)]
#[test]
fn stopping_nextest_asks_first_and_takes_the_answer() {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;

    // Ends by itself after a minute, should a test leave it behind.
    let mut child = Command::new("sh")
        .args(["-c", "trap 'exit 7' TERM; printf X; sleep 60; exit 3"])
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdout.take().unwrap().read_exact(&mut [0]).unwrap();

    let grace = Duration::from_secs(30);
    let started = Instant::now();
    stop_nextest(&mut child, grace);
    let status = child.try_wait().unwrap().expect("runner was reaped");
    // It ended the way it chose to when asked, and at once: nothing waited for the
    // grace to run out before asking.
    assert_eq!(status.code(), Some(7));
    assert!(started.elapsed() < grace);
}

#[cfg(unix)]
#[test]
fn stopping_nextest_stops_the_groups_its_tests_run_in() {
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt as _;

    // Stands for nextest with one test: the test runs in a group of its own, and
    // neither of the two ends when asked to. Both end by themselves after a minute.
    let dir = scratch("nextest-groups");
    let source = dir.join("runner.rs");
    let exe = dir.join("runner");
    fs::write(
        &source,
        r#"
use std::os::unix::process::CommandExt;
extern "C" {
    fn signal(signal: i32, handler: usize) -> usize;
}
fn main() {
    // Ignored (SIG_IGN), which what is started from here inherits.
    unsafe { signal(15, 1) };
    let test = std::process::Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
    println!("{}", test.id());
    std::thread::sleep(std::time::Duration::from_secs(60));
}
"#,
    )
    .unwrap();
    let output = Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut child = Command::new(&exe)
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let test: u32 = line.trim().parse().unwrap();

    stop_nextest(&mut child, Duration::from_millis(10));
    let reaped = child.try_wait().unwrap();
    let stopped = ends(test);
    fs::remove_dir_all(dir).unwrap();
    assert!(reaped.is_some(), "runner was not reaped");
    assert!(stopped, "the group of the test was left running");
}

#[cfg(unix)]
fn process(pid: u32, parent: u32, group: u32, started: &str) -> TestProcess {
    TestProcess {
        pid,
        parent,
        group,
        started: started.to_string(),
    }
}

#[cfg(unix)]
#[test]
fn the_processes_of_a_runner_are_its_descendants_in_groups_they_lead() {
    let table = || {
        vec![
            // Listed before its parent: one pass over the table does not find it.
            process(30, 20, 30, "t"),
            process(20, 10, 10, "t"),
            process(10, 1, 10, "t"),
            // A child that joined a group of somebody else.
            process(40, 10, 77, "t"),
            process(77, 1, 77, "t"),
            // Somebody else's process that joined the group of the runner.
            process(50, 1, 10, "t"),
            process(60, 1, 60, "t"),
        ]
    };
    assert_eq!(
        owned_among(table(), 10),
        [
            process(30, 20, 30, "t"),
            process(20, 10, 10, "t"),
            process(10, 1, 10, "t")
        ]
    );
    // A runner that is gone has nothing.
    assert!(owned_among(table(), 11).is_empty());
}

#[cfg(unix)]
#[test]
fn a_group_survives_only_through_the_very_process_seen_in_it() {
    let owned = [process(10, 1, 10, "a"), process(30, 10, 30, "b")];
    let surviving = |current: &[TestProcess]| surviving_groups(&owned, current);
    assert_eq!(
        surviving(&[
            process(10, 1, 10, "a"),
            process(30, 1, 30, "b"),
            process(60, 1, 60, "a")
        ]),
        BTreeSet::from([10, 30])
    );
    // The number of a process that ended, given to a later one.
    assert_eq!(surviving(&[process(10, 1, 10, "later")]), BTreeSet::new());
    // A process that left for a group which was never the runner's.
    assert_eq!(surviving(&[process(30, 1, 99, "b")]), BTreeSet::new());
    // Another process of the same second in the group, which nobody saw there.
    assert_eq!(surviving(&[process(31, 1, 30, "b")]), BTreeSet::new());
}

#[test]
fn a_working_directory_that_cannot_be_inspected_is_not_created_over() {
    let dir = scratch("marks-below-a-file");
    let file = dir.join("file");
    fs::write(&file, "").unwrap();
    let error = work_directory(&file, "marks").unwrap_err();
    assert!(error.to_string().starts_with("inspecting "), "{error:#}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_error_is_blamed_on_the_file_it_names() {
    let copy = Path::new("/work");
    let make = |name: &str, source: &str, first_id| {
        let sites = instrument::discover(source, first_id).unwrap().sites;
        let rendered = instrument::render(source, &sites, &|_| true);
        SourceFile {
            display: name.to_string(),
            path: copy.join(name),
            owners: BTreeSet::new(),
            lines: scope::LineIndex::new(source),
            original: source.to_string(),
            places: vec![],
            sites,
            rebuild_sites: vec![],
            rendered: Some(rendered),
        }
    };
    let files = [
        make("first.rs", "pub fn f(n: u32) -> bool { n >= 4 }", 0),
        make("second.rs", "pub fn g(a: u8, b: u8) -> bool { a < b }", 100),
    ];
    let rendered = files[1].rendered.as_ref().unwrap();
    let probe = rendered.text.find("__tmt_rt::").unwrap();
    let blamed = rendered.inserted_by(probe).unwrap();
    assert!(blamed >= 100, "{blamed}");
    let error = CompileError {
        code: None,
        message: String::new(),
        primary: vec![ErrorSpan {
            file: "second.rs".to_string(),
            byte_start: probe,
            line: 1,
        }],
        secondary: vec![],
        rendered: String::new(),
    };
    assert_eq!(blame_error(&error, &files, copy, &HashSet::new()), [blamed]);
}

#[test]
fn non_probe_errors_are_detected_and_never_blamed() {
    let copy = Path::new("/copy");
    let source = "pub fn f(n: u32) -> bool { n >= 4 }\nconst S: &str = include_str!(\"../../missing.json\");\n";
    let mut found = instrument::discover(source, 0).unwrap();
    let rendered = instrument::render(source, &found.sites, &|_| true);
    let files = [SourceFile {
        display: "src/lib.rs".into(),
        path: copy.join("src/lib.rs"),
        owners: BTreeSet::new(),
        original: source.into(),
        lines: scope::LineIndex::new(source),
        sites: std::mem::take(&mut found.sites),
        rebuild_sites: vec![],
        places: vec![],
        rendered: Some(rendered),
    }];
    let span = ErrorSpan {
        file: "src/lib.rs".into(),
        byte_start: 0,
        line: 2,
    };
    let cases = [
        (
            None,
            "couldn't read `src/../../missing.json`: No such file or directory",
            "error: couldn't read `src/../../missing.json`: No such file or directory",
        ),
        (
            None,
            "environment variable `MISSING_VAR` not defined at compile time",
            "error: environment variable `MISSING_VAR` not defined at compile time",
        ),
        (
            Some("E0583"),
            "file not found for module `ghost`",
            "error[E0583]: file not found for module `ghost`",
        ),
    ];
    for (code, message, rendered) in cases {
        let error = CompileError {
            code: code.map(str::to_string),
            message: message.to_string(),
            primary: vec![span.clone()],
            secondary: vec![],
            rendered: rendered.to_string(),
        };
        assert!(is_non_probe_error(&error, &files, copy), "{message}");
        assert!(blame_error(&error, &files, copy, &HashSet::new()).is_empty());
    }

    // An env! error originating inside probe-inserted text (e.g., --error 'env!("TMT_UNSET")')
    // must still be treated as a probe error and blamed so repair can disable it.
    let probe_offset = files[0]
        .rendered
        .as_ref()
        .unwrap()
        .text
        .find("__tmt_rt::")
        .unwrap();
    let probe_env_error = CompileError {
        code: None,
        message: "environment variable `TMT_UNSET` not defined at compile time".into(),
        primary: vec![ErrorSpan {
            file: "src/lib.rs".into(),
            byte_start: probe_offset,
            line: 1,
        }],
        secondary: vec![],
        rendered: "error: environment variable `TMT_UNSET` not defined at compile time".into(),
    };
    assert!(!is_non_probe_error(&probe_env_error, &files, copy));
    assert!(!blame_error(&probe_env_error, &files, copy, &HashSet::new()).is_empty());
}

#[test]
fn stage_external_parses_and_validates_parent_relative_specs() {
    let shorthand: StageExternal = "../../proto".parse().unwrap();
    assert_eq!(shorthand.from, PathBuf::from("../../proto"));
    assert_eq!(shorthand.rel, PathBuf::from("../../proto"));

    let explicit: StageExternal = "/abs/shared/proto=../../proto".parse().unwrap();
    assert_eq!(explicit.from, PathBuf::from("/abs/shared/proto"));
    assert_eq!(explicit.rel, PathBuf::from("../../proto"));

    for bad in [
        "",
        "proto",
        "./proto",
        "../..",
        "/abs/dest",
        "src=/abs/dest",
        "=../../proto",
        "../../proto\n",
    ] {
        assert!(
            bad.parse::<StageExternal>().is_err(),
            "should reject {bad:?}"
        );
    }
}

#[test]
fn test_selection_matches_pict_table() {
    let target = |name: &str, kind: &str| Target {
        name: name.to_string(),
        kind: vec![kind.to_string()],
        src_path: PathBuf::from("src/lib.rs"),
        test: true,
        doctest: false,
        required_features: vec![],
    };
    let pkg = Package {
        name: "my-api".to_string(),
        dependencies: vec![],
        features: BTreeMap::new(),
        manifest_path: PathBuf::from("workspace/Cargo.toml"),
        targets: vec![
            target("my_api", "lib"),
            target("my-api", "bin"),
            target("smoke", "test"),
        ],
    };
    let tests = [
        ("presentation::pagination::page_one", false),
        ("presentation::pagination::page_two", false),
        ("domain::auth::ignored_case", true),
    ];
    for row in include_str!("../../tests/data/test-selection-filter.pict.tsv")
        .lines()
        .skip(1)
    {
        let cols: Vec<_> = row.split('\t').collect();
        let &[selector, filter, include, exclude, ignored] = cols.as_slice() else {
            panic!("invalid PICT row: {row}");
        };
        let mut sel = TestSelection::default();
        match selector {
            "none" => {}
            "lib" => sel.lib = true,
            "bins" => sel.bins = true,
            "integration_test" => sel.integration_tests.push("smoke".into()),
            "test_target_lib" => sel.targets.push("my_api".into()),
            "test_target_bin" => sel.targets.push("my-api".into()),
            _ => panic!("unknown selector: {selector}"),
        }
        match filter {
            "none" => {}
            "matching_prefix" => sel.filter.push("presentation::pagination".into()),
            "matching_multi" => {
                sel.filter.push("page_one".into());
                sel.filter.push("ignored_case".into());
            }
            "unmatched" => sel.filter.push("nonexistent::mod".into()),
            _ => panic!("unknown filter: {filter}"),
        }
        match include {
            "none" => {}
            "matching" => sel
                .include
                .push("presentation::pagination::page_one".into()),
            "unmatched" => sel.include.push("presentation::pagination".into()),
            _ => panic!("unknown include: {include}"),
        }
        match exclude {
            "none" => {}
            "matching" => sel
                .exclude
                .push("presentation::pagination::page_one".into()),
            _ => panic!("unknown exclude: {exclude}"),
        }
        sel.include_ignored = ignored == "true";

        assert!(validate_target_selection(&[&pkg], &sel).is_ok(), "{row}");
        let build_flags = test_build_flags(
            &[&pkg],
            &sel,
            Path::new("workspace"),
            &CargoConfig::default(),
        );
        match selector {
            "none" => assert_eq!(build_flags, None, "{row}"),
            "lib" | "test_target_lib" => {
                assert_eq!(build_flags, Some(vec!["--lib".into()]), "{row}")
            }
            "bins" | "test_target_bin" => {
                assert_eq!(build_flags, Some(vec!["--bin=my-api".into()]), "{row}")
            }
            "integration_test" => {
                assert_eq!(build_flags, Some(vec!["--test=smoke".into()]), "{row}")
            }
            _ => unreachable!(),
        }

        let matched: Vec<&str> = tests
            .iter()
            .filter(|(name, is_ign)| sel.matches_test(name, *is_ign))
            .map(|(name, _)| *name)
            .collect();
        if exclude == "matching" {
            assert!(
                !matched.contains(&"presentation::pagination::page_one"),
                "{row}"
            );
        }
        if !sel.include_ignored {
            assert!(!matched.contains(&"domain::auth::ignored_case"), "{row}");
        }
        if filter == "unmatched" && include != "matching" {
            assert!(matched.is_empty(), "{row}");
        }
        if selector == "test_target_bin" {
            let hint =
                no_tests_matched_hint(&[&pkg], &sel, &[("my-api".into(), "my-api".into())], &[]);
            assert!(hint.contains("--lib") && hint.contains("my_api"), "{hint}");
        }
        if include == "unmatched" && filter == "none" {
            let discovered: Vec<String> = tests.iter().map(|(n, _)| (*n).to_string()).collect();
            let hint = no_tests_matched_hint(&[&pkg], &sel, &[], &discovered);
            assert!(hint.contains("--test-filter"), "{hint}");
        }
    }
}

#[test]
fn validate_target_selection_rejects_missing_targets_before_build() {
    let pkg = Package {
        name: "my-api".to_string(),
        dependencies: vec![],
        features: BTreeMap::new(),
        manifest_path: PathBuf::from("workspace/Cargo.toml"),
        targets: vec![Target {
            name: "my_api".to_string(),
            kind: vec!["lib".to_string()],
            src_path: PathBuf::from("src/lib.rs"),
            test: true,
            doctest: false,
            required_features: vec![],
        }],
    };
    let bins_err = validate_target_selection(
        &[&pkg],
        &TestSelection {
            bins: true,
            ..TestSelection::default()
        },
    )
    .unwrap_err();
    assert!(bins_err.to_string().contains("--bins"), "{bins_err}");

    let int_err = validate_target_selection(
        &[&pkg],
        &TestSelection {
            integration_tests: vec!["missing".into()],
            ..TestSelection::default()
        },
    )
    .unwrap_err();
    assert!(int_err.to_string().contains("--test missing"), "{int_err}");

    let hyphen_err = validate_target_selection(
        &[&pkg],
        &TestSelection {
            targets: vec!["my-api".into()],
            ..TestSelection::default()
        },
    )
    .unwrap_err();
    assert!(hyphen_err.to_string().contains("--lib"), "{hyphen_err}");
}

#[test]
fn build_progress_tracks_compiled_and_cached_artifacts() {
    let mut state = BuildProgressState::default();
    assert_eq!(state.format_line(), "building: compiling...");
    state.observe_line(
        r#"{"reason":"compiler-artifact","target":{"name":"serde","kind":["lib"]},"fresh":true}"#,
    );
    assert_eq!(state.format_line(), "building: 1 cached (latest: serde)");
    state.observe_line(
        r#"{"reason":"compiler-artifact","target":{"name":"aws-sdk-s3","kind":["lib"]},"fresh":false}"#,
    );
    assert_eq!(
        state.format_line(),
        "building: 1 compiled (1 cached, latest: aws-sdk-s3)"
    );

    #[cfg(unix)]
    {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            r#"printf '%s\n' '{"reason":"compiler-artifact","target":{"name":"clap","kind":["lib"]},"fresh":true}'; sleep 0.12"#,
        ]);
        let mut lines = Vec::new();
        let output = run_cargo_with_progress_interval(
            &mut cmd,
            true,
            std::time::Duration::from_millis(30),
            |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(
            lines.len() >= 2,
            "expected repeated progress ticks: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("s elapsed)")),
            "expected elapsed suffix on repeated line: {lines:?}"
        );
    }
}

#[test]
fn repair_cache_matches_pict_table() {
    let dir = scratch("repair-cache-pict");
    for (idx, row) in include_str!("../../tests/data/repair-cache.pict.tsv")
        .lines()
        .skip(1)
        .enumerate()
    {
        let cols: Vec<_> = row.split('\t').collect();
        let &[stage_ext, err_kind, cache_state, src_changed] = cols.as_slice() else {
            panic!("invalid PICT row: {row}");
        };
        let work = dir.join(format!("row-{idx}"));
        fs::create_dir_all(&work).unwrap();

        match stage_ext {
            "none" => {}
            "shorthand_parent" => {
                let parsed: StageExternal = "../../proto".parse().unwrap();
                assert_eq!(parsed.rel, PathBuf::from("../../proto"));
            }
            "explicit_mapping" => {
                let parsed: StageExternal = "/abs/proto=../../proto".parse().unwrap();
                assert_eq!(parsed.from, PathBuf::from("/abs/proto"));
            }
            _ => panic!("unknown stage_ext: {stage_ext}"),
        }

        let err = match err_kind {
            "none" => None,
            "probe_error" => Some(CompileError {
                code: Some("E0308".into()),
                message: "mismatched types".into(),
                primary: vec![],
                secondary: vec![],
                rendered: "error[E0308]: mismatched types".into(),
            }),
            "include_str_missing" => Some(CompileError {
                code: None,
                message: "couldn't read `../../missing`: No such file or directory".into(),
                primary: vec![],
                secondary: vec![],
                rendered: "error: couldn't read `../../missing`: No such file or directory".into(),
            }),
            "env_missing" => Some(CompileError {
                code: None,
                message: "environment variable `FOO` not defined at compile time".into(),
                primary: vec![],
                secondary: vec![],
                rendered: "error: environment variable `FOO` not defined at compile time".into(),
            }),
            "mod_missing" => Some(CompileError {
                code: Some("E0583".into()),
                message: "file not found for module `bar`".into(),
                primary: vec![],
                secondary: vec![],
                rendered: "error[E0583]: file not found for module `bar`".into(),
            }),
            _ => panic!("unknown err_kind: {err_kind}"),
        };
        if let Some(err) = &err {
            let expected_non_probe = err_kind != "probe_error";
            assert_eq!(
                is_non_probe_error(err, &[], Path::new("/copy")),
                expected_non_probe,
                "{row}"
            );
        }

        let fp_old = "fp-old";
        let fp_cur = if src_changed == "true" {
            "fp-new"
        } else {
            fp_old
        };
        let site1 = repair_cache::CachedSite {
            file: "src/lib.rs".into(),
            site_index: 0,
            start: 10,
            end: 20,
            text: "a < b".into(),
            kind: "Cmp".into(),
            reason: "E0308".into(),
        };
        match cache_state {
            "miss" => {}
            "hit_exact" | "hit_stale_partial" => {
                repair_cache::save(&work, fp_old, std::slice::from_ref(&site1)).unwrap();
            }
            "corrupt" => {
                fs::write(work.join(".tmt-repair-cache.json"), "{not-valid-json").unwrap();
            }
            _ => panic!("unknown cache_state: {cache_state}"),
        }

        let loaded = repair_cache::load(&work, fp_cur);
        if (cache_state == "hit_exact" || cache_state == "hit_stale_partial")
            && src_changed == "false"
        {
            assert_eq!(loaded, vec![site1], "{row}");
        } else {
            assert!(loaded.is_empty(), "{row}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn repair_cache_save_does_not_overwrite_hardlinked_outside_file() {
    let dir = scratch("repair-cache-hardlink");
    let work = dir.join("work");
    fs::create_dir_all(&work).unwrap();
    let victim = dir.join("victim.txt");
    fs::write(&victim, "original-secret").unwrap();
    fs::hard_link(&victim, work.join(".tmt-repair-cache.json")).unwrap();
    fs::hard_link(&victim, work.join(".tmt-repair-cache.json.new")).unwrap();

    repair_cache::save(&work, "fp", &[]).unwrap();
    assert_eq!(fs::read_to_string(&victim).unwrap(), "original-secret");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn stage_external_rejects_symlink_escaping_staged_root() {
    let dir = scratch("stage-external-escape");
    let ext = dir.join("ext");
    let work = dir.join("work");
    fs::create_dir_all(&ext).unwrap();
    fs::create_dir_all(work.join("src")).unwrap();
    let secret = dir.join("secret.txt");
    fs::write(&secret, "top-secret").unwrap();
    std::os::unix::fs::symlink(&secret, ext.join("leak.txt")).unwrap();

    let state = WorkCopy::open(&work, &work.join("src"));
    let err = copy_external_entry(
        &ext,
        &work.join("src/ext"),
        None,
        &work,
        &|_| false,
        &state,
        &mut BTreeSet::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("escapes its staged root"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stage_external_case_insensitive_collision_is_detected() {
    assert!(paths_overlap_case_insensitive(
        Path::new("_parent_0"),
        Path::new("_PARENT_0")
    ));
    assert!(paths_overlap_case_insensitive(
        Path::new("apps/api"),
        Path::new("APPS")
    ));
    assert!(!paths_overlap_case_insensitive(
        Path::new("apps/api"),
        Path::new("proto")
    ));
}

#[test]
fn build_progress_handles_non_utf8_stdout_identically_to_quiet() {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf '{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"demo\",\"kind\":[\"lib\"]}}\\n\\377\\376\\n'"]);
        let mut lines = Vec::new();
        let out =
            run_cargo_with_progress_interval(&mut cmd, true, Duration::from_secs(60), |line| {
                lines.push(line.to_string())
            })
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.windows(2).any(|w| w == [0xff, 0xfe]));
    }
}

#[test]
fn modules_are_followed_to_files_that_exist_in_the_copy() {
    let scratch = scratch("module-files");
    let copy = scratch.join("work/src");
    let library = copy.join("lib.rs");
    let directory_module = copy.join("nested/mod.rs");
    let outside = scratch.join("work/outside.rs");
    let sources = BTreeMap::from([
        (
            library.clone(),
            r#"mod nested; #[path = "../outside.rs"] mod outside;"#,
        ),
        (directory_module.clone(), "fn nested() {}"),
        (outside.clone(), "fn outside() {}"),
    ]);
    for path in sources.keys() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(path).unwrap();
    }
    let graph = source_graph([(library.clone(), true)], &copy, |path| {
        Ok(sources.get(path).map(|source| (*source).to_string()))
    })
    .unwrap();
    fs::remove_dir_all(scratch).unwrap();
    // `nested.rs` comes first among the places of `mod nested;` and is not there;
    // `outside.rs` is there, but is not a file of the copy.
    assert_eq!(
        graph.keys().collect::<Vec<_>>(),
        [&library, &directory_module]
    );
}

#[test]
fn an_empty_directory_the_run_asked_for_is_not_pruned() {
    let dir = scratch("prune-kept-directory");
    let copy = dir.join("work/src");
    fs::create_dir(copy.join("fixture")).unwrap();
    fs::create_dir(copy.join("stale")).unwrap();
    let state = WorkCopy::open(&dir.join("work"), &copy);
    // As the copy of a fixture directory without files leaves it.
    state.kept.borrow_mut().insert(copy.join("fixture"));
    prune(&copy, &state).unwrap();
    assert!(copy.join("fixture").is_dir());
    assert!(!copy.join("stale").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pool_with_one_worker_runs_nothing_after_the_first_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = AtomicUsize::new(0);
    let failed = run_pool(3, 1, |index| {
        calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            Err(anyhow!("job {index} broke"))
        } else {
            Ok(index)
        }
    });
    assert_eq!(failed.unwrap_err().to_string(), "job 0 broke");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_checkpoint_gets_every_verdict_once_it_is_final() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = scratch("checkpointed-verdicts");
    let keys = || vec!["first".to_string(), "second".to_string()];
    for serial in [false, true] {
        let checkpoint = Checkpoint::open(&dir.join(format!("checkpoint-{serial}"))).unwrap();
        let runs = AtomicUsize::new(0);
        // The first mutant runs out of time next to the other and passes alone.
        let run = |index: usize, sharing: usize| {
            runs.fetch_add(1, Ordering::SeqCst);
            let timed_out = index == 0 && sharing > 1;
            Ok(JobResult {
                first_pass_timeout: false,
                unconfirmed_timeout: false,
                survived: (!timed_out).then(|| vec![index.to_string()]),
                timed_out,
                unable: false,
            })
        };
        checkpoint.start("campaign", serde_json::json!({})).unwrap();
        let phase = checkpoint.begin("mutants", "context", keys()).unwrap();
        // Finishing the phase fails when a verdict was not recorded, and recording
        // fails when one was recorded twice.
        let results = evaluate_jobs(2, 2, serial, false, "test", Some(&phase), &run).unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), if serial { 3 } else { 2 });
        assert_eq!(results[0].unconfirmed_timeout, !serial);
        assert_eq!(results[0].survived.is_some(), serial);
        // The phase was finished, or the campaign could not be.
        checkpoint.finish().unwrap();

        // The campaign started again has nothing left to run.
        checkpoint.start("campaign", serde_json::json!({})).unwrap();
        let phase = checkpoint.begin("mutants", "context", keys()).unwrap();
        let again = evaluate_jobs(2, 2, serial, false, "test", Some(&phase), &run).unwrap();
        assert_eq!(again, results);
        assert_eq!(runs.load(Ordering::SeqCst), if serial { 3 } else { 2 });
        checkpoint.finish().unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn copy_entry_pict_table() {
    use std::os::unix::fs::symlink;

    let dir = scratch("copy-entry-pict");
    let workspace = dir.join("ws");
    let outside = dir.join("outside");
    let copy = dir.join("copy");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&copy).unwrap();
    let ws_canon = workspace.canonicalize().unwrap();

    let rows = [
        ("symlink", "file", "outside", "no", true),
        ("symlink", "file", "inside", "yes", false),
        ("symlink", "dir", "outside", "no", false),
        ("symlink", "dir", "inside", "no", true),
        ("symlink", "special", "inside", "no", false),
        ("special", "none", "inside", "no", false),
        ("file", "none", "inside", "no", true),
        ("symlink", "file", "inside", "no", true),
        ("symlink", "dir", "outside", "yes", false),
        ("dir", "none", "inside", "yes", true),
        ("symlink", "special", "outside", "no", false),
        ("symlink", "dir", "inside", "yes", false),
        ("symlink", "dangling", "inside", "no", false),
        ("dir", "none", "inside", "no", true),
        ("file", "none", "inside", "yes", true),
        ("symlink", "file", "outside", "yes", false),
    ];

    for (index, (entry_kind, target_kind, location, skipped, should_copy)) in
        rows.into_iter().enumerate()
    {
        let case_ws = workspace.join(format!("case-{index}"));
        let case_out = outside.join(format!("case-{index}"));
        let case_dst = copy.join(format!("case-{index}"));
        fs::create_dir_all(&case_ws).unwrap();
        fs::create_dir_all(&case_out).unwrap();
        let src = case_ws.join("entry");
        let mut skip_target = None;

        match entry_kind {
            "file" => fs::write(&src, "plain").unwrap(),
            "dir" => {
                fs::create_dir_all(&src).unwrap();
                fs::write(src.join("inner.txt"), "inner").unwrap();
            }
            "special" => {
                let status = Command::new("mkfifo").arg(&src).status().unwrap();
                assert!(status.success());
            }
            "symlink" => {
                let base = if location == "outside" {
                    &case_out
                } else {
                    &case_ws
                };
                let target = base.join("target");
                match target_kind {
                    "file" => fs::write(&target, "linked").unwrap(),
                    "dir" => {
                        fs::create_dir_all(&target).unwrap();
                        fs::write(target.join("inner.txt"), "inner").unwrap();
                    }
                    "special" => {
                        let status = Command::new("mkfifo").arg(&target).status().unwrap();
                        assert!(status.success());
                    }
                    "dangling" => {}
                    other => panic!("unexpected target_kind {other}"),
                }
                if skipped == "yes" && target_kind != "dangling" {
                    skip_target = Some(target.canonicalize().unwrap());
                }
                symlink(&target, &src).unwrap();
            }
            other => panic!("unexpected entry_kind {other}"),
        }

        fs::create_dir_all(&case_dst).unwrap();
        let state = WorkCopy::open(&case_dst, &case_dst);
        let dst = case_dst.join("entry");
        let skip = |path: &Path| skip_target.as_ref().is_some_and(|blocked| blocked == path);
        copy_entry(&src, &dst, &ws_canon, &skip, &state).unwrap();
        assert_eq!(
            dst.exists(),
            should_copy,
            "row {index}: {entry_kind}/{target_kind}/{location}/{skipped}"
        );
    }
    // A symlink pointing to a file nested under a skipped ancestor directory is also skipped.
    let nested_ws = workspace.join("skipped-ancestor");
    let blocked_dir = nested_ws.join("blocked");
    fs::create_dir_all(&blocked_dir).unwrap();
    let nested_file = blocked_dir.join("secret.txt");
    fs::write(&nested_file, "blocked").unwrap();
    let link = nested_ws.join("link.txt");
    symlink(&nested_file, &link).unwrap();
    let blocked_canon = blocked_dir.canonicalize().unwrap();
    let nested_dst = copy.join("skipped-ancestor");
    fs::create_dir_all(&nested_dst).unwrap();
    let state = WorkCopy::open(&nested_dst, &nested_dst);
    copy_entry(
        &link,
        &nested_dst.join("link.txt"),
        &ws_canon,
        &|path| path == blocked_canon,
        &state,
    )
    .unwrap();
    assert!(!nested_dst.join("link.txt").exists());
    // A workspace whose own outer ancestor has a skipped name (e.g. `node_modules`)
    // still copies symlinks pointing to files inside the workspace.
    let outer_ws = dir.join("node_modules/pkg");
    fs::create_dir_all(&outer_ws).unwrap();
    let real = outer_ws.join("real.txt");
    fs::write(&real, "ok").unwrap();
    let inner_link = outer_ws.join("alias.txt");
    symlink(&real, &inner_link).unwrap();
    let outer_ws_canon = outer_ws.canonicalize().unwrap();
    let outer_dst = copy.join("outer-ancestor");
    fs::create_dir_all(&outer_dst).unwrap();
    let state = WorkCopy::open(&outer_dst, &outer_dst);
    let skip_node_modules =
        |path: &Path| path.file_name().is_some_and(|name| name == "node_modules");
    copy_entry(
        &inner_link,
        &outer_dst.join("alias.txt"),
        &outer_ws_canon,
        &skip_node_modules,
        &state,
    )
    .unwrap();
    assert!(outer_dst.join("alias.txt").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_verdict_pict_table() {
    let rows = [
        ("pass", "unable", "source_twin"),
        ("fail", "unable", "source_twin"),
        ("unconfirmed_timeout", "pass", "source_twin"),
        ("pass", "fail", "source_twin"),
        ("pass", "unable", "all_tests"),
        ("pass", "fail", "all_tests"),
        ("pass", "pass", "all_tests"),
        ("fail", "pass", "source_twin"),
        ("fail", "fail", "source_twin"),
        ("fail", "timeout", "all_tests"),
        ("unconfirmed_timeout", "timeout", "source_twin"),
        ("pass", "pass", "source_twin"),
        ("unable", "unable", "all_tests"),
        ("unable", "timeout", "all_tests"),
        ("fail", "timeout", "source_twin"),
        ("unable", "fail", "all_tests"),
        ("unable", "fail", "source_twin"),
        ("unconfirmed_timeout", "unable", "all_tests"),
        ("pass", "timeout", "source_twin"),
        ("unable", "pass", "source_twin"),
        ("unable", "unviable", "source_twin"),
        ("unable", "unable", "source_twin"),
        ("pass", "timeout", "all_tests"),
        ("fail", "fail", "all_tests"),
        ("fail", "pass", "all_tests"),
        ("unconfirmed_timeout", "fail", "all_tests"),
        ("unconfirmed_timeout", "unviable", "source_twin"),
        ("unconfirmed_timeout", "pass", "all_tests"),
        ("pass", "unviable", "source_twin"),
        ("unconfirmed_timeout", "timeout", "all_tests"),
        ("unconfirmed_timeout", "fail", "source_twin"),
        ("fail", "unable", "all_tests"),
        ("unable", "timeout", "source_twin"),
        ("fail", "unviable", "source_twin"),
        ("unconfirmed_timeout", "unable", "source_twin"),
        ("unable", "pass", "all_tests"),
    ];
    let job_of = |state: &str| JobResult {
        first_pass_timeout: false,
        unconfirmed_timeout: state == "unconfirmed_timeout",
        survived: (state == "pass").then(|| vec!["bin".to_string()]),
        timed_out: state == "unconfirmed_timeout",
        unable: state == "unable",
    };
    let outcome_of = |outcome: &str| match outcome {
        "unviable" => None,
        "pass" => Some(Outcome::Pass),
        "fail" => Some(Outcome::Fail),
        "timeout" => Some(Outcome::Timeout),
        "unable" => Some(Outcome::Unable),
        other => panic!("unexpected outcome {other}"),
    };
    for (probe_state, built_outcome, mode) in rows {
        let probe = job_of(probe_state);
        match mode {
            "all_tests" => {
                let again = JobResult {
                    first_pass_timeout: false,
                    unconfirmed_timeout: false,
                    survived: (built_outcome == "pass").then(|| vec!["bin".to_string()]),
                    timed_out: built_outcome == "timeout",
                    unable: built_outcome == "unable",
                };
                let expected = matches!(built_outcome, "fail" | "timeout");
                assert_eq!(
                    audit_all_tests_disagrees(&again),
                    expected,
                    "{probe_state}/{built_outcome}/{mode}"
                );
            }
            "source_twin" => {
                let built = outcome_of(built_outcome);
                let expected = if matches!(probe_state, "unable" | "unconfirmed_timeout")
                    || matches!(built_outcome, "unviable" | "unable")
                {
                    None
                } else {
                    Some((built_outcome == "pass") != (probe_state == "pass"))
                };
                assert_eq!(
                    audit_source_comparison(&probe, built),
                    expected,
                    "{probe_state}/{built_outcome}/{mode}"
                );
            }
            other => panic!("unexpected mode {other}"),
        }
    }
    assert!(!audit_all_tests_disagrees(&job_of("unconfirmed_timeout")));
}

#[test]
fn wrapper_relative_rejects_escape_and_control_characters() {
    assert_eq!(
        wrapper_relative(Path::new("./scripts/wrap.sh")).unwrap(),
        PathBuf::from("scripts/wrap.sh")
    );
    for bad in [
        "",
        ".",
        "../wrap.sh",
        "a/../wrap.sh",
        "/usr/bin/env",
        "scripts/wrap\0.sh",
        "scripts/wrap\n.sh",
    ] {
        assert!(wrapper_relative(Path::new(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn stronger_assertions_includes_boundary_mutants() {
    let source = "\
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_only_above_boundary() {
        let _ = is_adult(18);
        assert!(is_adult(20));
    }
}
";
    let parsed = syn::parse_file(source).unwrap();
    let found = instrument::discover(source, 0).unwrap();
    assert!(!found.sites.is_empty());
    let candidates = stronger_assertions(
        source,
        &parsed,
        found.sites.iter().chain(&found.rebuild_sites),
    );
    assert!(
        candidates
            .iter()
            .any(|c| c.description.contains("age > 18")),
        "expected strict boundary candidate `age > 18`, got: {:?}",
        candidates
            .iter()
            .map(|c| c.description.as_str())
            .collect::<Vec<_>>()
    );
}

#[cfg(unix)]
#[test]
fn shared_witness_preserves_original_binary_across_mutant_overwrite() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = std::env::temp_dir().join(format!("tmt-shared-witness-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let marks = root.join("marks");
    fs::create_dir_all(&marks).unwrap();
    let exe_path = root.join("deps_test_bin");
    let write_script = |body: &str| {
        fs::write(&exe_path, body).unwrap();
        fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755)).unwrap();
    };

    let baseline = TestBinary {
        name: "unit".into(),
        cwd: root.clone(),
        marks: marks.clone(),
        git_ceiling: root.clone(),
        work_lock: None,
        tests: BTreeSet::new(),
        runner: Runner::Isolated {
            exe: exe_path.clone(),
            test: "unit".into(),
            ignored: false,
            wrapper: None,
        },
        baseline: Duration::from_millis(50),
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::ZERO,
        timeout: Some(Duration::from_secs(5)),
        wrapper_grace: Duration::from_millis(100),
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    };

    let mut scenario = witness::Scenario::default();
    scenario.expression = "f(1)".into();
    let make_candidate = || witness::Candidate {
        edits: vec![],
        scope: 1..=1,
        signature_lines: vec![1],
        description: "m".into(),
        existing: false,
        advice: None,
        file: PathBuf::from("src/lib.rs"),
        scenarios: vec![scenario.clone()],
        finding_at: None,
    };
    let c1 = make_candidate();
    let c2 = make_candidate();
    let survivors = vec![
        WitnessSurvivor {
            candidate: &c1,
            at: report::Location::default(),
            existing: None,
            scenarios: vec![(&c1.scenarios[0], &baseline)],
        },
        WitnessSurvivor {
            candidate: &c2,
            at: report::Location::default(),
            existing: None,
            scenarios: vec![(&c2.scenarios[0], &baseline)],
        },
    ];

    let build_count = std::cell::Cell::new(0usize);
    let originals = shared_witnesses(
        "fn f(x: i32) -> i32 { x }\n",
        &survivors,
        None,
        true,
        |_edits| {
            let n = build_count.get();
            build_count.set(n + 1);
            if n == 0 {
                write_script("#!/bin/sh\nprintf '42\\n' >> \"$TMT_WITNESS_OUTPUT\"\n");
            } else {
                write_script("#!/bin/sh\nprintf '42\\ttrue\\n' >> \"$TMT_WITNESS_OUTPUT\"\n");
            }
            Ok(Some(exe_path.clone()))
        },
        |_| {},
    )
    .unwrap();

    let shared = originals[0].as_ref().expect("shared witness");
    let saved_path = shared.original_exe.path.clone();
    assert_ne!(saved_path, exe_path);
    assert!(saved_path.exists());

    // Simulate Cargo overwriting the exact same output path when building a mutant.
    write_script("#!/bin/sh\nprintf '99\\tfalse\\n' >> \"$TMT_WITNESS_OUTPUT\"\n");

    let mutant = observe_witness(
        &baseline,
        exe_path.clone(),
        None,
        &c1.scenarios[0],
        &shared.name,
        true,
        true,
    )
    .unwrap()
    .expect("mutant observation");
    assert_eq!(mutant.value, "99");
    assert_eq!(mutant.equals_expected, Some(false));

    let rechecked = observe_witness(
        &baseline,
        saved_path.clone(),
        None,
        &c1.scenarios[0],
        &shared.name,
        true,
        true,
    )
    .unwrap()
    .expect("preserved original observation");
    assert_eq!(rechecked.value, "42");
    assert_eq!(rechecked.equals_expected, Some(true));

    drop(originals);
    assert!(
        !saved_path.exists(),
        "preserved witness binary should be cleaned up on drop"
    );

    // Colliding pre-existing files and symlinks at candidate backup paths must be preserved.
    let pid = std::process::id();
    let existing_file = root.join(format!("tmt-orig-{pid}-0-deps_test_bin"));
    let symlink_target = root.join("keep_me");
    let existing_link = root.join(format!("tmt-orig-{pid}-1-deps_test_bin"));
    fs::write(&existing_file, "preexisting").unwrap();
    fs::write(&symlink_target, "link-target").unwrap();
    std::os::unix::fs::symlink(&symlink_target, &existing_link).unwrap();

    let counter = std::sync::atomic::AtomicUsize::new(0);
    let saved = SavedExecutable::copy_from_with_counter(&exe_path, &counter).unwrap();
    let expected_slot = root.join(format!("tmt-orig-{pid}-2-deps_test_bin"));
    assert_eq!(saved.path, expected_slot);
    assert_eq!(fs::read_to_string(&existing_file).unwrap(), "preexisting");
    assert_eq!(fs::read_to_string(&symlink_target).unwrap(), "link-target");
    drop(saved);
    assert!(!expected_slot.exists());
    assert_eq!(fs::read_to_string(&existing_file).unwrap(), "preexisting");
    assert_eq!(fs::read_to_string(&symlink_target).unwrap(), "link-target");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_report_prefix_preserves_literal_backslashes_on_unix_and_keeps_crate_api() {
    let found = crate::instrument::discover_with_errors("fn f() -> bool { true }", 0, &[]).unwrap();
    assert!(!found.sites.is_empty());
    assert_eq!(workspace_report_prefix(Path::new("")), "");
    assert_eq!(
        workspace_report_prefix(Path::new("crates/core")),
        "crates/core/"
    );
    #[cfg(unix)]
    assert_eq!(
        workspace_report_prefix(Path::new(r"crates/a\b")),
        r"crates/a\b/"
    );
}

#[test]
fn describe_effects_skips_unknown_test_marker_in_multi_test_list() {
    let source = "fn f(s: &mut S) -> bool { s.reset(); s.count > 1 }";
    let discovered = instrument::discover(source, 0).unwrap();
    let sites: BTreeMap<u32, (&str, &Site)> = discovered
        .sites
        .iter()
        .map(|s| (s.id, ("src/lib.rs", s)))
        .collect();
    let set = |items: &[Change]| items.iter().cloned().collect::<BTreeSet<_>>();

    let stmt_id = discovered
        .sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Stmt))
        .expect("stmt site")
        .id;
    let cmp_id = discovered
        .sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Cmp { .. }))
        .expect("cmp site")
        .id;

    // UNKNOWN_TEST ("?") sorts before alphabetic test names in BTreeMap, so mixed lists
    // must pick the first named test as representative and never display raw `?`.
    let mut mixed = BTreeMap::new();
    mixed.insert(
        UNKNOWN_TEST.to_string(),
        set(&[Change::NowRuns { site: stmt_id }]),
    );
    mixed.insert(
        "tests::alpha".to_string(),
        set(&[Change::NowRuns { site: stmt_id }]),
    );
    mixed.insert(
        "tests::beta".to_string(),
        set(&[Change::NowRuns { site: stmt_id }]),
    );
    let lines = describe_effects(&mixed, &sites);
    assert_eq!(
        lines,
        ["with the mutant, `tests::alpha` (and 2 more) now runs `s.reset();` (src/lib.rs:1)"]
    );
    assert!(lines.iter().all(|line| !line.contains("`?`")));

    // Single UNKNOWN_TEST still renders as "a test".
    let mut only_unknown = BTreeMap::new();
    only_unknown.insert(
        UNKNOWN_TEST.to_string(),
        set(&[Change::NoLongerRuns { site: cmp_id }]),
    );
    assert_eq!(
        describe_effects(&only_unknown, &sites),
        ["with the mutant, a test no longer runs `s.count > 1` (src/lib.rs:1)"]
    );
}
