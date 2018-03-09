extern crate gocar;

fn main() {
    let files = gocar::scan_c_files("example_c_project/src/main.c").unwrap();
    //println!("{:?}", files);
    let need_recompile = gocar::ModifiedSources::scan("example_c_project/main", &files).unwrap();

    let mut empty = true;
    for path in need_recompile {
        let path = path.unwrap();
        empty = false;

        println!("Compiling: {:?}", path);
        if !std::process::Command::new("c++")
            .arg("-W")
            .arg("-Wall")
            .arg("-c")
            .arg("-o")
            .arg(gocar::unit_to_obj(path).unwrap())
            .arg(path)
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success() {
                panic!("Compilation failed")
        }
    }

    if empty {
        println!("Binary up to date");
        return;
    }

    if !std::process::Command::new("c++")
        .arg("-W")
        .arg("-Wall")
        .arg("-o")
        .arg("example_c_project/main")
        .args(files.iter().map(|(file, _)| gocar::unit_to_obj(file).unwrap()))
        .spawn()
        .unwrap()
        .wait()
        .unwrap()
        .success() {
            panic!("Compilation failed")
    }
}
