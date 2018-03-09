extern crate gocar;

fn main() {
    let files = gocar::scan_c_files("example_c_project/src/main.c").unwrap();
    println!("{:?}", files);
    let need_recompile = gocar::need_recompile("example_c_project/main", &files).unwrap();
    if need_recompile.len() == 0 {
        println!("Binary up to date");
        return;
    }
    println!("Compiling: {:?}", need_recompile);

    for &path in &need_recompile {
        if !std::process::Command::new("c++")
            .arg("-W")
            .arg("-Wall")
            .arg("-c")
            .arg("-o")
            .arg(gocar::unit_to_obj(path))
            .arg(path)
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success() {
                panic!("Compilation failed")
        }
    }

    if !std::process::Command::new("c++")
        .arg("-W")
        .arg("-Wall")
        .arg("-o")
        .arg("example_c_project/main")
        .args(files.iter().map(|(file, _)| gocar::unit_to_obj(file)))
        .spawn()
        .unwrap()
        .wait()
        .unwrap()
        .success() {
            panic!("Compilation failed")
    }
}
