extern crate gocar;
extern crate toml;

fn load_config() -> gocar::Project {
    use std::io::Read;

    let mut config = Vec::new();
    std::fs::File::open("Gocar.toml")
        .unwrap()
        .read_to_end(&mut config)
        .unwrap();

    let mut config = toml::from_slice::<gocar::Project>(&config).unwrap();
    config.init_default_profiles();
    //println!("Config: {:?}", config);
    config
}

fn build(profile: &str) {
    let config = load_config();
    let target = AsRef::<std::path::Path>::as_ref("target").join(profile);
    std::fs::create_dir_all(&target).unwrap();
    config.build(&target, profile).unwrap();
}

fn test(profile: &str) {
    let config = load_config();

    let mut target = AsRef::<std::path::Path>::as_ref("target").join(profile);
    target.push("integration_tests");
    let profile = config.profiles.get(profile).expect("unknown profile");
    //println!("Testing with profile: {:?}", profile);

    let mut test_count = 0;
    let mut fail_count = 0;

    std::fs::create_dir_all(&target).unwrap();
    for test in std::fs::read_dir("tests").unwrap().map(Result::unwrap).map(|e| e.path()) {
        let extension_is_valid = if let Some(extension) = test.extension() {
            extension == "c" || extension == "cpp"
        } else {
            continue;
        };

        let test_name: std::path::PathBuf = test.file_stem().unwrap().into();
        if extension_is_valid {
            let binary = gocar::Binary {
                name: test_name.clone(),
                root_file: test,
                compile_options: vec!["-DGOCAR_INTEFRATION_TEST".into()],
                link_options: Vec::new(),
            };

            test_count += 1;

            binary.build(&target, profile).unwrap();
            let test_binary = target.join(&test_name);
            println!("     \u{1B}[32;1mRunning\u{1B}[0m {:?}", test_binary);

            if !std::process::Command::new(&test_binary)
                .spawn().unwrap()
                .wait().unwrap()
                .success() {
                    fail_count += 1;
                    println!("      \u{1B}[31;1mFailed\u{1B}[0m {:?}", test_binary);
            }
        }
    }

    println!("test result: {}. total: {}; passed: {}; failed: {}", if fail_count == 0 { "\u{1B}[32mok\u{1B}[0m" } else { "\u{1B}[31mFAILED\u{1B}[0m" }, test_count, test_count - fail_count, fail_count);
}

fn main() {
    let mut args = std::env::args();
    args.next().expect("Not even zeroth argument given");
    let action = args.next().expect("Usage: gocar (build [--release] | run [--release] | test)");

    let profile = if let Some("--release") = args.next().as_ref().map(AsRef::as_ref) {
        "release"
    } else {
        "debug"
    };

    match action.as_ref() {
        "build" => build(profile),
        "run" => unimplemented!(),
        "test" => test(profile),
        _ => panic!("Unknown action: {}", action),
    }
}
