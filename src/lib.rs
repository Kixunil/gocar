extern crate serde;
#[macro_use]
extern crate serde_derive;

use std::collections::{HashMap, HashSet};
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::ffi::OsString;

mod objs;

struct HeaderExtractor<R: BufRead> {
    reader: std::iter::Filter<std::iter::Map<io::Split<R>, fn(io::Result<Vec<u8>>) -> io::Result<Vec<u8>>>, fn(&io::Result<Vec<u8>>) -> bool>,
}

fn drop_lf(item: io::Result<Vec<u8>>) -> io::Result<Vec<u8>> {
    item.map(|mut item| { if item.last() == Some(&b'\n') { item.pop(); } item })
}

fn filter_headers(item: &io::Result<Vec<u8>>) -> bool {
    match *item {
        Ok(ref item) => item.ends_with(b".h") || item.ends_with(b".hpp"),
        Err(_) => true,
    }
}

impl<R: BufRead> HeaderExtractor<R> {
    pub fn new(reader: R) -> Self {
        HeaderExtractor {
            reader: reader
                .split(b' ')
                .map(drop_lf as fn(io::Result<Vec<u8>>) -> io::Result<Vec<u8>>)
                .filter(filter_headers as fn(&io::Result<Vec<u8>>) -> bool)
        }
    }
}

impl<R: BufRead> Iterator for HeaderExtractor<R> {
    type Item = io::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::os::unix::ffi::OsStringExt;

        self.reader.next().map(|item| item.map(std::ffi::OsString::from_vec).map(Into::into))
    }
}

fn header_to_unit<'a, P: AsRef<Path> + Into<PathBuf>, I: 'a + IntoIterator<Item=&'a DetachedHeaders>>(path: P, mappings: I) -> Option<PathBuf> {
    let mut path = path.into();
    path.set_extension("c");

    let c_exists = path.exists();
    path.set_extension("cpp");
    if path.exists() {
        if c_exists {
            None
        } else {
            Some(path)
        }
    } else {
        if c_exists {
            path.set_extension("c");
            Some(path)
        } else {
            for mapping in mappings {
                if let Ok(stripped) = path.strip_prefix(&mapping.includes) {
                    let mut path = mapping.sources.join(stripped);
                    return if path.exists() {
                        Some(path)
                    } else {
                        path.set_extension("c");
                        if path.exists() {
                            Some(path)
                        } else {
                            None
                        }
                    };
                }
            }
            None
        }
    }
}

/// Convert path to a .c(pp) file to a path to .o file.
fn unit_to_obj<P: AsRef<Path> + Into<PathBuf>>(path: P) -> Option<PathBuf> {
    path.as_ref().extension()?;
    let mut path = path.into();
    path.set_extension("o");
    Some(path)
}

fn get_headers<P: AsRef<Path>>(file: P, profile: &Profile) -> io::Result<Vec<PathBuf>> {
    let (compiler, compiler_specific_options) = if file.as_ref().extension().map_or(false, |e| e == "c") {
        (&profile.c_compiler, &profile.c_compile_options)
    } else {
        (&profile.cpp_compiler, &profile.cpp_compile_options)
    };

    let mut cpp = std::process::Command::new(compiler)
        .args(compiler_specific_options)
        .args(&profile.compile_options)
        .arg("-MM")
        .arg(file.as_ref())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let headers = HeaderExtractor::new(io::BufReader::new(cpp.stdout.take().expect("Stdout not set")));
    let headers = headers.collect();
    if !cpp.wait()?.success() {
        return Err(io::ErrorKind::Other.into());
    }
    headers
}

/// Scans files in the project
fn scan_c_files<P: AsRef<Path> + Into<PathBuf>, I: IntoIterator<Item=P>>(root_files: I, profile: &Profile, project: &Project, ignore_files: &HashSet<PathBuf>, strip_dir: &Path) -> io::Result<HashMap<PathBuf, Vec<PathBuf>>> {
        let detached_headers = project.detached_headers.iter().map(|mapping| Ok(DetachedHeaders { includes: mapping.includes.canonicalize()?, sources: mapping.sources.canonicalize()?})).collect::<io::Result<Vec<_>>>()?;
    let mut scanned_files = root_files.into_iter().map(|file| {
        println!("\u{1B}[32;1m    Scanning\u{1B}[0m {:?}", file.as_ref().strip_prefix(strip_dir).unwrap_or(file.as_ref()));
        get_headers(&file, profile).map(|headers| (file.into(), headers))
    }).collect::<Result<HashMap<_, _>, _>>()?;

    let mut prev_file_count = 0;

    while prev_file_count != scanned_files.len() {
        prev_file_count = scanned_files.len();
        let candidates = scanned_files
            .iter()
            .flat_map(|(_, headers)| headers.iter())
            .filter_map(|header| {
                let unit = header_to_unit(header.canonicalize().unwrap(), &detached_headers);
                if !project.ignore_missing_sources && unit.is_none() {
                    panic!("Missing source for header {:?}")
                }
                unit
            })
            .filter(|file| !scanned_files.contains_key(file))
            .filter(|file| !ignore_files.contains(file))
            .collect::<Vec<_>>();

        let candidates = candidates.into_iter().map(|file| {
            println!("\u{1B}[32;1m    Scanning\u{1B}[0m {:?}", file.strip_prefix(strip_dir).unwrap_or(&file));
            let headers = get_headers(&file, profile);
            (file, headers)
        });

        for (file, headers) in candidates {
            let headers = headers?;
            scanned_files.insert(file, headers);
        }
    }

    Ok(scanned_files)
}

fn is_older<P: AsRef<Path>, I: Iterator<Item=P>>(time: std::time::SystemTime, files: I) -> io::Result<bool> {
    for file in files {
        if std::fs::metadata(&file)?.modified()? > time {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Iterator over modified sources
struct ModifiedSources<'a> {
    target_time: Option<std::time::SystemTime>,
    sources: std::collections::hash_map::Iter<'a, PathBuf, Vec<PathBuf>>,
}

impl<'a> ModifiedSources<'a> {
    pub fn scan<P: AsRef<Path>>(target: P, sources: &'a HashMap<PathBuf, Vec<PathBuf>>) -> io::Result<Self> {
        let target_time = match std::fs::metadata(target) {
            Ok(metadata) => Some(metadata.modified()?),
            Err(err) => if err.kind() == io::ErrorKind::NotFound {
                None
            } else {
                return Err(err);
            },
        };

        Ok(ModifiedSources {
            target_time,
            sources: sources.iter(),
        })
    }
}

impl<'a> Iterator for ModifiedSources<'a> {
    type Item = io::Result<&'a Path>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (source, headers) = self.sources.next()?;
            if let Some(target_time) = self.target_time {
                match is_older(target_time, Some(source).into_iter().chain(headers)) {
                    Ok(true) => return Some(Ok(source)),
                    Ok(false) => (),
                    Err(err) => return Some(Err(err)),
                }
            } else {
                return Some(Ok(source))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Binary {
    pub name: PathBuf,
    pub root_files: HashSet<PathBuf>,
    #[serde(default)]
    pub compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub link_options: Vec<PathBuf>,
    #[serde(default)]
    pub ignore_files: HashSet<PathBuf>,
}

impl Binary {
    pub fn build<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile: &Profile, project: &Project) -> io::Result<()> {
        //println!("Profile: {:?}", profile);
        let bin_path = target_dir.as_ref().join(&self.name);
        let strip_prefix = std::env::current_dir().unwrap_or_else(|_| PathBuf::new());
        let ignore_files = self.ignore_files.iter().map(|path| path.canonicalize()).collect::<Result<_, _>>()?;
        let files = scan_c_files(&self.root_files, profile, project, &ignore_files, &strip_prefix)?;
        let need_recompile = ModifiedSources::scan(&bin_path, &files)?;

        let mut empty = true;
        let mut has_cpp = false;
        for path in need_recompile {
            let path = path?;
            empty = false;

            let output = objs::get_obj_path(&target_dir, &project_dir, unit_to_obj(path).unwrap());
            std::fs::create_dir_all(output.parent().unwrap())?;
            println!("   \u{1B}[32;1mCompiling\u{1B}[0m {:?}", output.strip_prefix(&strip_prefix).unwrap_or(&output));
            let (compiler, compiler_specific_options) = if path.extension().map_or(false, |ext| ext == "c") {
                (&profile.c_compiler, &profile.c_compile_options)
            } else {
                has_cpp = true;
                (&profile.cpp_compiler, &profile.cpp_compile_options)
            };

            if !std::process::Command::new(compiler)
                .args(&profile.compile_options)
                .args(compiler_specific_options)
                .args(&self.compile_options)
                .arg("-c")
                .arg("-o")
                .arg(&output)
                .arg(path)
                .spawn()?
                .wait()?
                .success() {
                    print!("      \u{1B}[31;1mFailed\u{1B}[0m {:?}", compiler);
                    for arg in &profile.compile_options {
                        print!(" {:?}", arg);
                    }
                    for arg in compiler_specific_options {
                        print!(" {:?}", arg);
                    }
                    for arg in &self.compile_options {
                        print!(" {:?}", arg);
                    }
                    println!(" -c -o {:?} {:?}", output, path);
                    return Err(io::ErrorKind::Other.into());
            }

            if let Some(post_compile) = &project.post_compile {
                println!("\u{1B}[32;1mPost compile\u{1B}[0m {:?}", output.strip_prefix(&strip_prefix).unwrap_or(&output));
                if !std::process::Command::new(post_compile)
                    .arg(&output)
                    .arg(path)
                    .arg(compiler)
                    .args(&profile.compile_options)
                    .args(compiler_specific_options)
                    .args(&self.compile_options)
                    .spawn()?
                    .wait()?
                    .success() {
                        print!("      \u{1B}[31;1mFailed\u{1B}[0m {:?} {:?} {:?} {:?}", post_compile, output, path, compiler);
                        for arg in &profile.compile_options {
                            print!(" {:?}", arg);
                        }
                        for arg in compiler_specific_options {
                            print!(" {:?}", arg);
                        }
                        for arg in &self.compile_options {
                            print!(" {:?}", arg);
                        }
                        println!();
                        return Err(io::ErrorKind::Other.into());
                }
            }
        }

        if empty {
            println!("  \u{1B}[32;1mUp to date\u{1B}[0m {:?}", bin_path.strip_prefix(&strip_prefix).unwrap_or(&bin_path));
            return Ok(());
        }

        let linker = if has_cpp {
            &profile.cpp_compiler
        } else {
            &profile.c_compiler
        };

        println!("     \u{1B}[32;1mLinking\u{1B}[0m {:?}", bin_path.strip_prefix(&strip_prefix).unwrap_or(&bin_path));
        if std::process::Command::new(linker)
            .args(&self.link_options)
            .arg("-o")
            .arg(&bin_path)
            .args(files.iter().map(|(file, _)| objs::get_obj_path(&target_dir, &project_dir, unit_to_obj(file).unwrap())))
            .spawn()?
            .wait()?
            .success() {
                Ok(())
        } else {
            Err(io::ErrorKind::Other.into())
        }
    }
}

fn default_c_compiler() -> PathBuf {
	std::env::var_os("CC").map_or_else(|| "cc".to_owned().into(), Into::into)
}

fn default_cpp_compiler() -> PathBuf {
	std::env::var_os("CXX").map_or_else(|| "c++".to_owned().into(), Into::into)
}

#[derive(Debug, Deserialize, Clone)]
pub struct Profile {
    #[serde(default = "default_c_compiler")]
    pub c_compiler: PathBuf,
    #[serde(default = "default_cpp_compiler")]
    pub cpp_compiler: PathBuf,
    #[serde(default)]
    pub compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub c_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub cpp_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub link_options: Vec<PathBuf>,
}

impl Profile {
    pub fn release() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: vec!["-O2".into()],
            c_compile_options: Vec::new(),
            cpp_compile_options: Vec::new(),
            link_options: Vec::new(),
        }
    }

    pub fn debug() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: vec!["-g".into(), "-DDEBUG".into()],
            c_compile_options: Vec::new(),
            cpp_compile_options: Vec::new(),
            link_options: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DetachedHeaders {
    includes: PathBuf,
    sources: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub bin: Vec<Binary>,
    #[serde(default)]
    pub profiles: std::collections::HashMap<String, Profile>,
    #[serde(default)]
    pub add_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub add_c_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub add_cpp_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub add_link_options: Vec<PathBuf>,
    #[serde(default)]
    pub ignore_missing_sources: bool,
    #[serde(default)]
    pub detached_headers: Vec<DetachedHeaders>,
    #[serde(default)]
    pub post_compile: Option<PathBuf>,
}

impl Project {
    pub fn init_default_profiles(&mut self) {
        self.profiles.entry("release".to_owned()).or_insert_with(Profile::release);
        self.profiles.entry("debug".to_owned()).or_insert_with(Profile::debug);
        for (_, profile) in &mut self.profiles {
            profile.compile_options.extend_from_slice(&self.add_compile_options);
            profile.c_compile_options.extend_from_slice(&self.add_c_compile_options);
            profile.cpp_compile_options.extend_from_slice(&self.add_cpp_compile_options);
            profile.link_options.extend_from_slice(&self.add_link_options);
        }
    }

    pub fn build<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile: &str) -> io::Result<()> {
        let profile = self.profiles.get(profile).ok_or(io::ErrorKind::InvalidInput)?;

        for bin in &self.bin {
            bin.build(&target_dir, &project_dir, &profile, &self)?;
        }
        Ok(())
    }
}
