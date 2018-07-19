extern crate serde;
#[macro_use]
extern crate serde_derive;

use std::collections::HashMap;
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::ffi::OsString;

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

fn header_to_unit<P: AsRef<Path> + Into<PathBuf>>(path: P) -> Option<PathBuf> {
    path.as_ref().extension()?;
    let mut path = path.into();
    path.set_extension("c");

    Some(if path.exists() {
        path
    } else {
        path.set_extension("cpp");
        path
    })
}

/// Convert path to a .c(pp) file to a path to .o file.
fn unit_to_obj<P: AsRef<Path> + Into<PathBuf>>(path: P) -> Option<PathBuf> {
    path.as_ref().extension()?;
    let mut path = path.into();
    path.set_extension("o");
    Some(path)
}

fn get_headers<P: AsRef<Path>>(file: P) -> io::Result<Vec<PathBuf>> {
    let mut cpp = std::process::Command::new("c++")
        .arg("-MM")
        .arg(file.as_ref())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let headers = HeaderExtractor::new(io::BufReader::new(cpp.stdout.take().expect("Stdout not set")));
    headers.collect()
}

/// Scans files in the project
pub fn scan_c_files<P: AsRef<Path>>(root_file: P) -> io::Result<HashMap<PathBuf, Vec<PathBuf>>> {
    let mut scanned_files = HashMap::new();

    let headers = get_headers(&root_file)?;
    scanned_files.insert(PathBuf::from(root_file.as_ref()), headers);

    let mut prev_file_count = 0;

    while prev_file_count != scanned_files.len() {
        prev_file_count = scanned_files.len();
        let candidates = scanned_files
            .iter()
            .flat_map(|(_, headers)| headers.iter())
            .map(header_to_unit)
            .map(Option::unwrap)
            .filter(|file| !scanned_files.contains_key(file))
            .collect::<Vec<_>>();

        for (file, headers) in candidates.into_iter().map(|file| { let headers = get_headers(&file); (file, headers) }) {
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
pub struct ModifiedSources<'a> {
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
    pub root_file: PathBuf,
    #[serde(default)]
    pub compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub link_options: Vec<PathBuf>,
}

impl Binary {
    pub fn build<P: AsRef<Path>>(&self, target_dir: P, profile: &Profile) -> io::Result<()> {
        let bin_path = target_dir.as_ref().join(&self.name);
        let files = scan_c_files(&self.root_file)?;
        let need_recompile = ModifiedSources::scan(&bin_path, &files)?;

        let mut empty = true;
        let mut has_cpp = false;
        for path in need_recompile {
            let path = path?;
            empty = false;

            let output = unit_to_obj(path).unwrap();
            println!("   \u{1B}[32;1mCompiling\u{1B}[0m {:?}", output);
            let (compiler, options) = if path.extension().map_or(false, |ext| ext == "c") {
                (&profile.c_compiler, &profile.compile_options)
            } else {
                has_cpp = true;
                (&profile.cpp_compiler, &profile.compile_options)
            };

            if !std::process::Command::new(compiler)
                .args(options)
                .args(&self.compile_options)
                .arg("-c")
                .arg("-o")
                .arg(output)
                .arg(path)
                .spawn()?
                .wait()?
                .success() {
                    return Err(io::ErrorKind::Other.into());
            }
        }

        if empty {
            println!("  \u{1B}[32;1mUp to date\u{1B}[0m {:?}", self.name);
            return Ok(());
        }

        let linker = if has_cpp {
            &profile.cpp_compiler
        } else {
            &profile.c_compiler
        };

        println!("     \u{1B}[32;1mLinking\u{1B}[0m {:?}", bin_path);
        if std::process::Command::new(linker)
            .args(&self.link_options)
            .arg("-o")
            .arg(&bin_path)
            .args(files.iter().map(|(file, _)| unit_to_obj(file).unwrap()))
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
    pub link_options: Vec<PathBuf>,
}

impl Profile {
    pub fn release() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: vec!["-O2".into()],
            link_options: Vec::new(),
        }
    }

    pub fn debug() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: vec!["-g".into(), "-DDEBUG".into()],
            link_options: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Project {
    pub bin: Vec<Binary>,
    #[serde(default)]
    pub profiles: std::collections::HashMap<String, Profile>,
    #[serde(default)]
    pub add_compile_options: Vec<PathBuf>,
    #[serde(default)]
    pub add_link_options: Vec<PathBuf>,
}

impl Project {
    pub fn init_default_profiles(&mut self) {
        self.profiles.entry("release".to_owned()).or_insert_with(Profile::release);
        self.profiles.entry("debug".to_owned()).or_insert_with(Profile::debug);
        for (_, profile) in &mut self.profiles {
            profile.compile_options.extend_from_slice(&self.add_compile_options);
            profile.link_options.extend_from_slice(&self.add_link_options);
        }
    }

    pub fn build<P: AsRef<Path>>(&self, target_dir: P, profile: &str) -> io::Result<()> {
        let profile = self.profiles.get(profile).ok_or(io::ErrorKind::InvalidInput)?;

        for bin in &self.bin {
            bin.build(&target_dir, &profile)?;
        }
        Ok(())
    }
}
