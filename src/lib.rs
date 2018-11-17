extern crate serde;
#[macro_use]
extern crate serde_derive;

use std::collections::{HashMap, HashSet};
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::ffi::{OsString, OsStr};
use std::time::SystemTime;

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
    let compiler = Compiler::determine_from_file(&file).expect("Unknown extension");
    let options = profile.compile_options.all(compiler);
    let compiler = profile.compiler(compiler);

    let mut cpp = std::process::Command::new(compiler)
        .args(options)
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
fn scan_c_files<P: AsRef<Path>, I: IntoIterator<Item=P>>(root_files: I, profile: &Profile, project: &Project, ignore_files: &HashSet<PathBuf>, strip_dir: &Path) -> io::Result<HashMap<PathBuf, Vec<PathBuf>>> {
        let detached_headers = project.detached_headers.iter().map(|mapping| Ok(DetachedHeaders { includes: mapping.includes.canonicalize()?, sources: mapping.sources.canonicalize()?})).collect::<io::Result<Vec<_>>>()?;
    let mut scanned_files = root_files.into_iter().map(|file| {
        let file = file.as_ref().canonicalize()?;
        println!("\u{1B}[32;1m    Scanning\u{1B}[0m {:?}", file.strip_prefix(strip_dir).unwrap_or(&file));
        get_headers(&file, profile).map(|headers| (file, headers))
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

fn is_older<P: AsRef<Path>, I: Iterator<Item=P>>(time: SystemTime, files: I) -> io::Result<bool> {
    for file in files {
        if std::fs::metadata(&file)?.modified()? > time {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Iterator over modified sources
struct ModifiedSources<'a> {
    target_time: Option<SystemTime>,
    sources: std::collections::hash_map::Iter<'a, PathBuf, Vec<PathBuf>>,
}

impl<'a> ModifiedSources<'a> {
    pub fn scan(target_time: Option<SystemTime>, sources: &'a HashMap<PathBuf, Vec<PathBuf>>) -> Self {
        ModifiedSources {
            target_time,
            sources: sources.iter(),
        }
    }
}

fn get_file_mtime<P: AsRef<Path>>(file: P) -> io::Result<Option<SystemTime>> {
    match std::fs::metadata(file) {
        Ok(metadata) => Ok(Some(metadata.modified()?)),
        Err(err) => if err.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(err) },
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

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum Compiler {
    C,
    Cpp,
}

impl Compiler {
    pub fn determine_from_file<P: AsRef<Path>>(file: P) -> Option<Self> {
        let ext = file.as_ref().extension()?;
        // Why not "C" as well? According to https://stackoverflow.com/a/1546107 it means C++ but I
        // find it highly confusing. I'm not supporting it until there's a big pressure.
        if ext == "c" {
            Some(Compiler::C)
        } else if ext == "cpp" || ext == "cc" || ext == "cxx" || ext == "CPP" || ext == "CC" || ext == "CXX" {
            Some(Compiler::Cpp)
        } else {
            None
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct CompileOptions {
    #[serde(rename = "compile_options")]
    #[serde(default)]
    pub common: Vec<PathBuf>,
    #[serde(rename = "c_compile_options")]
    #[serde(default)]
    pub c: Vec<PathBuf>,
    #[serde(rename = "cpp_compile_options")]
    #[serde(default)]
    pub cpp: Vec<PathBuf>,
}

impl CompileOptions {
    pub fn all(&self, compiler: Compiler) -> impl Iterator<Item=&PathBuf> + Clone {
        self.common.iter().chain(match compiler {
            Compiler::C => &self.c,
            Compiler::Cpp => &self.cpp,
        })
    }

    fn only_common(options: Vec<PathBuf>) -> Self {
        CompileOptions {
            common: options,
            c: Vec::new(),
            cpp: Vec::new(),
        }
    }

    pub fn release() -> Self {
        Self::only_common(vec!["-O2".into()])
    }

    pub fn debug() -> Self {
        Self::only_common(vec!["-g".into(), "-DDEBUG".into()])
    }
}

#[derive(Debug, Deserialize)]
pub struct OsSpec {
    bin_spec: TargetSpec,
    static_lib_spec: TargetSpec,
    dynamic_lib_spec: TargetSpec,
}

impl OsSpec {
    pub fn linux() -> Self {
        OsSpec {
            bin_spec: TargetSpec {
                extension: "".into(),
                required_compile_options: Default::default(),
                required_link_options: Default::default(),
            },
            static_lib_spec: TargetSpec {
                extension: "a".into(),
                required_compile_options: Default::default(),
                required_link_options: vec![],
            },
            dynamic_lib_spec: TargetSpec {
                extension: "so".into(),
                required_compile_options: CompileOptions {
                    common: vec!["-fPIC".into()],
                    c: Default::default(),
                    cpp: Default::default(),
                },
                required_link_options: vec!["-shared".into()],
            },
        }
    }
}

pub struct BuildEnv<'a> {
    pub project_dir: &'a Path,
    pub target_dir: &'a Path,
    pub strip_prefix: &'a Path,
    pub os: OsSpec,
    pub profile: &'a Profile,
    pub project: &'a Project,
}

#[derive(Debug, Deserialize)]
pub struct TargetSpec {
    extension: OsString,
    required_compile_options: CompileOptions,
    required_link_options: Vec<PathBuf>,
}

pub trait TargetKind {
    type TargetOptions;

    fn get_spec(os: &OsSpec, options: Self::TargetOptions) -> &TargetSpec;
}

#[derive(Debug)]
pub enum BinTarget {}

impl TargetKind for BinTarget {
    type TargetOptions = ();

    fn get_spec(os: &OsSpec, _options: Self::TargetOptions) -> &TargetSpec {
        &os.bin_spec
    }
}

pub enum LibraryType {
    Static,
    Dynamic,
}

#[derive(Debug)]
pub enum LibTarget {}

impl TargetKind for LibTarget {
    type TargetOptions = LibraryType;

    fn get_spec(os: &OsSpec, options: Self::TargetOptions) -> &TargetSpec {
        match options {
            LibraryType::Static => &os.static_lib_spec,
            LibraryType::Dynamic => &os.dynamic_lib_spec,
        }
    }
}

struct CompileOutput {
    files: HashMap<PathBuf, Vec<PathBuf>>,
    up_to_date: bool,
    has_cpp: bool,
}

#[derive(Debug, Deserialize)]
pub struct Target<K: TargetKind> {
    pub name: PathBuf,
    pub root_files: HashSet<PathBuf>,
    #[serde(flatten)]
    pub compile_options: CompileOptions,
    #[serde(default)]
    pub link_options: Vec<PathBuf>,
    #[serde(default)]
    pub ignore_files: HashSet<PathBuf>,
    #[serde(skip)]
    pub _phantom: std::marker::PhantomData<K>,
}

impl<K: TargetKind> Target<K> {
    fn compile(&self, env: &BuildEnv, skip_older: Option<SystemTime>, spec: &TargetSpec) -> io::Result<CompileOutput> {
        let ignore_files = self.ignore_files.iter().map(|path| path.canonicalize()).collect::<Result<_, _>>()?;
        let files = scan_c_files(&self.root_files, env.profile, env.project, &ignore_files, &env.strip_prefix)?;

        let mut up_to_date = true;
        let mut has_cpp = false;
        for path in ModifiedSources::scan(skip_older, &files) {
            let path = path?;
            up_to_date = false;

            let output = objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(path).unwrap());
            std::fs::create_dir_all(output.parent().unwrap())?;
            println!("   \u{1B}[32;1mCompiling\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
            let compiler = Compiler::determine_from_file(&path).expect("Unknown extension");
            has_cpp |= compiler == Compiler::Cpp;
            let compile_options = spec.required_compile_options.all(compiler).chain(env.profile.compile_options.all(compiler)).chain(self.compile_options.all(compiler));
            let compiler = env.profile.compiler(compiler);

            if !std::process::Command::new(compiler)
                .args(compile_options.clone())
                .arg("-c")
                .arg("-o")
                .arg(&output)
                .arg(path)
                .spawn()?
                .wait()?
                .success() {
                    print!("      \u{1B}[31;1mFailed\u{1B}[0m {:?}", compiler);
                    for arg in compile_options.clone() {
                        print!(" {:?}", arg);
                    }
                    println!(" -c -o {:?} {:?}", output, path);
                    return Err(io::ErrorKind::Other.into());
            }

            if let Some(post_compile) = &env.project.post_compile {
                println!("\u{1B}[32;1mPost compile\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
                if !std::process::Command::new(post_compile)
                    .arg(&output)
                    .arg(path)
                    .arg(compiler)
                    .args(compile_options.clone())
                    .spawn()?
                    .wait()?
                    .success() {
                        print!("      \u{1B}[31;1mFailed\u{1B}[0m {:?} {:?} {:?} {:?}", post_compile, output, path, compiler);
                        for arg in compile_options {
                            print!(" {:?}", arg);
                        }
                        println!();
                        return Err(io::ErrorKind::Other.into());
                }
            }
        }

        Ok(CompileOutput {
            files,
            up_to_date,
            has_cpp,
        })
    }
}

fn link_using_compiler<CP: AsRef<OsStr>, OP: AsRef<Path>, O: AsRef<OsStr>, I: IntoIterator<Item=O> + Clone>(compiler: CP, output: OP, options: I, files: &HashMap<PathBuf, Vec<PathBuf>>, env: &BuildEnv) -> io::Result<()> {
    let output = output.as_ref();

    println!("     \u{1B}[32;1mLinking\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
    if std::process::Command::new(&compiler)
        .args(options.clone())
        .arg("-o")
        .arg(&output)
        .args(files.clone().into_iter().map(|(file, _)| objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(file).unwrap())))
        .spawn()?
        .wait()?
        .success() {
            Ok(())
    } else {
        print!("      \u{1B}[31;1mFailed\u{1B}[0m {:?}", compiler.as_ref());
        for arg in options {
            print!(" {:?}", arg.as_ref());
        }
        print!(" -o {:?}", output);
        for file in files.into_iter().map(|(file, _)| objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(file).unwrap())) {
            print!(" {:?}", file);
        }
        println!();
        Err(io::ErrorKind::Other.into())
    }
}

#[derive(Debug, Deserialize)]
pub struct Binary {
    #[serde(flatten)]
    pub target: Target<BinTarget>,
}

impl Binary {
    pub fn build(&self, env: &BuildEnv) -> io::Result<()> {
        let mut bin_path = env.target_dir.join(&self.target.name);
        bin_path.set_extension(&env.os.bin_spec.extension);
        let target_mtime = get_file_mtime(&bin_path)?;
        let compiled = self.target.compile(env, target_mtime, &env.os.bin_spec)?;

        if compiled.up_to_date {
            println!("  \u{1B}[32;1mUp to date\u{1B}[0m {:?}", bin_path.strip_prefix(&env.strip_prefix).unwrap_or(&bin_path));
            return Ok(());
        }

        let compiler = if compiled.has_cpp {
            &env.profile.cpp_compiler
        } else {
            &env.profile.c_compiler
        };

        let link_options = env.os.bin_spec.required_link_options.iter().chain(&self.target.link_options);
        link_using_compiler(compiler, bin_path, link_options, &compiled.files, env)
    }
}

#[derive(Debug, Deserialize)]
pub struct Library {
    #[serde(flatten)]
    pub target: Target<LibTarget>,
    #[serde(default)]
    pub disallow_static: bool,
    #[serde(default)]
    pub disallow_dynamic: bool,
    #[serde(default)]
    pub public_headers: HashSet<PathBuf>,
}

impl Library {
    pub fn build(&self, env: &BuildEnv, linkage: LibraryType) -> io::Result<()> {
        let mut lib_path = env.target_dir.join(&self.target.name);
        let lib_spec = match linkage {
            LibraryType::Dynamic => &env.os.dynamic_lib_spec,
            LibraryType::Static => &env.os.static_lib_spec,
        };
        lib_path.set_extension(&lib_spec.extension);
        let target_mtime = get_file_mtime(&lib_path)?;

        let compiled = self.target.compile(env, target_mtime, lib_spec)?;

        if compiled.up_to_date {
            println!("  \u{1B}[32;1mUp to date\u{1B}[0m {:?}", lib_path.strip_prefix(&env.strip_prefix).unwrap_or(&lib_path));
            return Ok(());
        }

        let compiler = if compiled.has_cpp {
            &env.profile.cpp_compiler
        } else {
            &env.profile.c_compiler
        };

        let link_options = lib_spec.required_link_options.iter().chain(&self.target.link_options);

        match linkage {
            LibraryType::Dynamic => link_using_compiler(compiler, lib_path, link_options, &compiled.files, env),
            LibraryType::Static => Library::link_static(lib_path, link_options, &compiled.files, env),
        }
    }

    fn link_static<OP: AsRef<Path>, O: AsRef<OsStr>, I: IntoIterator<Item=O> + Clone>(output: OP, options: I, files: &HashMap<PathBuf, Vec<PathBuf>>, env: &BuildEnv) -> io::Result<()> {
        let output = output.as_ref();
        let mut args: OsString = "crs".into();
        for arg in options {
            args.push(arg);
        }

        println!("     \u{1B}[32;1mLinking\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
        if std::process::Command::new("ar")
            .arg(&args)
            .arg(&output)
            .args(files.clone().into_iter().map(|(file, _)| objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(file).unwrap())))
            .spawn()?
            .wait()?
            .success() {
                Ok(())
        } else {
            print!("      \u{1B}[31;1mFailed\u{1B}[0m ar {:?}", &args);
            for file in files.into_iter().map(|(file, _)| objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(file).unwrap())) {
                print!(" {:?}", file);
            }
            println!();
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

#[derive(Debug, Deserialize)]
pub struct Profile {
    #[serde(default = "default_c_compiler")]
    pub c_compiler: PathBuf,
    #[serde(default = "default_cpp_compiler")]
    pub cpp_compiler: PathBuf,
    #[serde(flatten)]
    pub compile_options: CompileOptions,
    #[serde(default)]
    pub link_options: Vec<PathBuf>,
}

impl Profile {
    pub fn release() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: CompileOptions::release(),
            link_options: Vec::new(),
        }
    }

    pub fn debug() -> Self {
        Profile {
            c_compiler: default_c_compiler(),
            cpp_compiler: default_cpp_compiler(),
            compile_options: CompileOptions::debug(),
            link_options: Vec::new(),
        }
    }

    pub fn compiler(&self, compiler: Compiler) -> &Path {
        match compiler {
            Compiler::C => &self.c_compiler,
            Compiler::Cpp => &self.cpp_compiler,
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
    pub lib: Vec<Library>,
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
            profile.compile_options.common.extend_from_slice(&self.add_compile_options);
            profile.compile_options.c.extend_from_slice(&self.add_c_compile_options);
            profile.compile_options.cpp.extend_from_slice(&self.add_cpp_compile_options);
            profile.link_options.extend_from_slice(&self.add_link_options);
        }
    }

    pub fn build<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile: &str) -> io::Result<()> {
        let profile = self.profiles.get(profile).ok_or(io::ErrorKind::InvalidInput)?;
        let strip_prefix = std::env::current_dir().unwrap_or_else(|_| PathBuf::new());

        let env = BuildEnv {
            target_dir: target_dir.as_ref(),
            project_dir: project_dir.as_ref(),
            profile,
            project: self,
            strip_prefix: &strip_prefix,
            os: OsSpec::linux(),
        };

        for lib in &self.lib {
            lib.build(&env, LibraryType::Dynamic)?;
        }

        for bin in &self.bin {
            bin.build(&env)?;
        }
        Ok(())
    }
}
