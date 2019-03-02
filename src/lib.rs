extern crate serde;
#[macro_use]
extern crate serde_derive;

use std::collections::{HashMap, HashSet};
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::ffi::{OsString, OsStr};
use std::time::SystemTime;
use std::fmt;

mod objs;

#[derive(Debug)]
pub struct FsError {
    path: PathBuf,
    error: io::Error,
    operation: &'static str,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
       write!(f, "failed to {} {}: {}", self.operation, self.path.display(), self.error)
    }
}

type FsResult<T> = Result<T, FsError>;

#[derive(Debug)]
pub enum Error {
    Filesystem(FsError),
    Unspecified(io::Error),
    InvalidProfileName,
    CompileError(Vec<OsString>),
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Error {
       Error::Unspecified(value)
    }
}

impl From<FsError> for Error {
    fn from(value: FsError) -> Error {
       Error::Filesystem(value)
    }
}

type GocarResult<T> = Result<T, Error>;

trait ResultExt: Sized {
    type Item: Sized;

    fn err_ctx<F: FnOnce() -> (PathBuf, &'static str)>(self, f: F) -> FsResult<Self::Item>;
}

impl<T> ResultExt for io::Result<T> {
    type Item = T;

    fn err_ctx<F: FnOnce() -> (PathBuf, &'static str)>(self, f: F) -> FsResult<Self::Item> {
        self.map_err(|error| {
            let (path, operation) = f();

            FsError {
                path,
                error,
                operation,
            }
        })
    }
}

fn file_open<P: AsRef<Path> + Into<PathBuf>>(path: P) -> FsResult<std::fs::File> {
    std::fs::File::open(&path).err_ctx(|| (path.into(), "open file"))
}

fn create_dir_all<P: AsRef<Path> + Into<PathBuf>>(path: P) -> FsResult<()> {
    std::fs::create_dir_all(&path).err_ctx(|| (path.into(), "create directory structure"))
}

fn canonicalize<P: AsRef<Path> + Into<PathBuf>>(path: P) -> FsResult<PathBuf> {
    path.as_ref().canonicalize().err_ctx(|| (path.into(), "canonicalize"))
}

fn canonicalize_custom_wd<P: AsRef<Path> + Into<PathBuf>, WD: AsRef<Path>>(path: P, working_dir: WD) -> FsResult<PathBuf> {
    if path.as_ref().is_relative() {
        let path = working_dir.as_ref().join(path);
        path.canonicalize().err_ctx(|| (path, "canonicalize"))
    } else {
        path.as_ref().canonicalize().err_ctx(|| (path.into(), "canonicalize"))
    }
}

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

fn get_headers<P: AsRef<Path> + Into<PathBuf>>(file: P, env: &BuildEnv) -> GocarResult<Vec<PathBuf>> {
    let compiler = Compiler::determine_from_file(&file).expect("Unknown extension");
    let options = env.profile.compile_options.all(compiler);
    let compiler = env.profile.compiler(compiler);

    let mut cpp = std::process::Command::new(compiler)
        .args(env.include_dirs)
        .args(options.clone())
        .arg("-MM")
        .arg(file.as_ref())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let headers = HeaderExtractor::new(io::BufReader::new(cpp.stdout.take().expect("Stdout not set")));
    let headers = headers.collect::<Result<_, _>>().map_err(Into::into);
    if !cpp.wait()?.success() {
        let (min, max) = options.size_hint();
        let size = max.unwrap_or(min);
        let mut cmdline: Vec<OsString> = Vec::with_capacity(size + 3);
        cmdline.push(compiler.into());
        cmdline.extend(env.include_dirs.iter().map(Into::into));
        cmdline.extend(options.map(Into::into));
        cmdline.push("-MM".into());
        cmdline.push(file.into().into());
        return Err(Error::CompileError(cmdline));
    }
    headers
}

/// Scans files in the project
fn scan_c_files<P: AsRef<Path> + Into<PathBuf>, I: IntoIterator<Item=P>>(root_files: I, ignore_files: &HashSet<PathBuf>, env: &BuildEnv) -> GocarResult<HashMap<PathBuf, Vec<PathBuf>>> {
        let detached_headers = env.project.detached_headers.iter().map(|mapping| Ok(DetachedHeaders { includes: canonicalize_custom_wd(&mapping.includes, env.project_dir)?, sources: canonicalize_custom_wd(&mapping.sources, env.project_dir)?})).collect::<FsResult<Vec<_>>>()?;
    let mut scanned_files = root_files.into_iter().map(|file: _| -> GocarResult<_> {
        let file = canonicalize_custom_wd(file, env.project_dir)?;

        println!("\u{1B}[32;1m    Scanning\u{1B}[0m {:?}", file.strip_prefix(env.project_dir).unwrap_or(&file));
        get_headers(&file, env).map(|headers| (file, headers)).map_err(Into::into)
    }).collect::<Result<HashMap<_, _>, _>>()?;

    let mut prev_file_count = 0;

    while prev_file_count != scanned_files.len() {
        prev_file_count = scanned_files.len();
        let candidates = scanned_files
            .iter()
            .flat_map(|(_, headers)| headers.iter())
            .filter_map(|header| {
                let canonicalized = canonicalize_custom_wd(header, env.project_dir).unwrap();
                if env.headers_only.contains(&canonicalized) {
                    None
                } else {
                    let unit = header_to_unit(canonicalized, &detached_headers);
                    if !env.project.ignore_missing_sources && unit.is_none() {
                        panic!("Missing source for header {:?}", header)
                    }
                    unit
                }
            })
            .filter(|file| !scanned_files.contains_key(file))
            .filter(|file| !ignore_files.contains(file))
            .collect::<Vec<_>>();

        let candidates = candidates.into_iter().map(|file| {
            println!("\u{1B}[32;1m    Scanning\u{1B}[0m {:?}", file.strip_prefix(env.project_dir).unwrap_or(&file));
            let headers = get_headers(&file, env);
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

fn get_file_mtime<P: AsRef<Path>>(file: P) -> FsResult<Option<SystemTime>> {
    match std::fs::metadata(&file) {
        Ok(metadata) => Ok(Some(metadata.modified().err_ctx(|| (file.as_ref().to_owned(), "get modification time of"))?)),
        Err(err) => if err.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(err) },
    }
    .err_ctx(|| (file.as_ref().to_owned(), "get metadata of"))
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
    pub include_dir: &'a Path,
    pub include_dirs: &'a [OsString],
    pub lib_dirs: &'a [OsString],
    pub libs: &'a [OsString],
    pub strip_prefix: &'a Path,
    pub os: OsSpec,
    pub profile: &'a Profile,
    pub project: &'a Project,
    pub headers_only: &'a HashSet<PathBuf>,
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

#[derive(Copy, Clone, Debug, Deserialize)]
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
    fn compile(&self, env: &BuildEnv, skip_older: Option<SystemTime>, spec: &TargetSpec) -> GocarResult<CompileOutput> {
        let ignore_files = self.ignore_files.iter().map(canonicalize).collect::<Result<_, _>>()?;
        let files = scan_c_files(&self.root_files, &ignore_files, env)?;

        let mut up_to_date = true;
        let mut has_cpp = false;
        for path in ModifiedSources::scan(skip_older, &files) {
            let path = path?;
            up_to_date = false;

            let output = objs::get_obj_path(&env.target_dir, &env.project_dir, unit_to_obj(path).unwrap());
            create_dir_all(output.parent().unwrap())?;
            println!("   \u{1B}[32;1mCompiling\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
            let compiler = Compiler::determine_from_file(&path).expect("Unknown extension");
            has_cpp |= compiler == Compiler::Cpp;
            let mut include_param = OsString::from("-I");
            include_param.push(env.include_dir);
            let include_param: PathBuf = include_param.into();
            let compile_options = spec.required_compile_options
                .all(compiler)
                .chain(env.profile.compile_options.all(compiler))
                .chain(self.compile_options.all(compiler))
                .chain(std::iter::once(&include_param));

            let compiler = env.profile.compiler(compiler);

            if !std::process::Command::new(compiler)
                .args(env.include_dirs)
                .args(compile_options.clone())
                .arg("-c")
                .arg("-o")
                .arg(&output)
                .arg(path)
                .spawn()?
                .wait()?
                .success() {
                    let (min, max) = compile_options.size_hint();
                    let len = max.unwrap_or(min) + env.include_dirs.len() + 5;
                    let mut cmdline: Vec<OsString> = Vec::with_capacity(len);
                    cmdline.push(compiler.into());
                    cmdline.extend(env.include_dirs.iter().map(Into::into));
                    cmdline.extend(compile_options.clone().map(Into::into));
                    cmdline.push("-c".into());
                    cmdline.push("-o".into());
                    cmdline.push(output.into());
                    cmdline.push(path.into());

                    return Err(Error::CompileError(cmdline));
            }

            if let Some(post_compile) = &env.project.post_compile {
                println!("\u{1B}[32;1mPost compile\u{1B}[0m {:?}", output.strip_prefix(&env.strip_prefix).unwrap_or(&output));
                if !std::process::Command::new(post_compile)
                    .arg(&output)
                    .arg(path)
                    .arg(compiler)
                    .args(env.include_dirs)
                    .args(compile_options.clone())
                    .spawn()?
                    .wait()?
                    .success() {
                        let (min, max) = compile_options.size_hint();
                        let len = max.unwrap_or(min) + env.include_dirs.len() + 4;
                        let mut cmdline: Vec<OsString> = Vec::with_capacity(len);
                        cmdline.push(post_compile.into());
                        cmdline.push(output.into());
                        cmdline.push(path.into());
                        cmdline.push(compiler.into());
                        cmdline.extend(env.include_dirs.iter().map(Into::into));
                        cmdline.extend(compile_options.clone().map(Into::into));

                        return Err(Error::CompileError(cmdline));
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
        .args(env.lib_dirs)
        .args(env.libs)
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
    pub fn build(&self, env: &BuildEnv) -> GocarResult<()> {
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
        link_using_compiler(compiler, bin_path, link_options, &compiled.files, env).map_err(Into::into)
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
    pub fn build(&self, env: &BuildEnv, linkage: LibraryType) -> GocarResult<()> {
        let mut lib_name = OsString::from("lib");
        lib_name.push(&self.target.name);
        let mut lib_path = env.target_dir.join(&lib_name);
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
        .map_err(Into::into)
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
pub struct Dependency {
    path: PathBuf,
    #[serde(default)]
    linkage: Option<LibraryType>,
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
    #[serde(default)]
    pub headers_only: HashSet<PathBuf>,
    #[serde(default)]
    pub dependencies: HashMap<String, Dependency>,
    #[serde(default)]
    pub include_dirs: Vec<PathBuf>,
}

impl Project {
    pub fn load_from_dir<P: AsRef<Path>>(directory: P) -> GocarResult<Self> {
        use std::io::Read;

        let file_path = directory.as_ref().join("Gocar.toml");

        let mut project_data = Vec::new();
        file_open(file_path)?.read_to_end(&mut project_data)?;
        let mut project = toml::from_slice::<Project>(&project_data).unwrap();
        project.init_default_profiles();
        Ok(project)
    }

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

    pub fn build_dependencies<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile_name: &str, linkage: LibraryType) -> GocarResult<(PathBuf, Vec<OsString>, Vec<OsString>)> {
        let include_dir = [target_dir.as_ref(), "deps".as_ref(), "include".as_ref()].iter().collect::<PathBuf>();
        let mut lib_dirs = Vec::with_capacity(self.dependencies.len());
        let mut libs = Vec::with_capacity(self.dependencies.len());

        for (dep_name, dep) in &self.dependencies {
            let project = Project::load_from_dir(&dep.path)?;
            let dep_lib_dir = [target_dir.as_ref(), "deps".as_ref(), "lib".as_ref(), dep_name.as_ref()].iter().collect::<PathBuf>();
            let dep_include_dir = include_dir.join(&dep_name);
            create_dir_all(&dep_lib_dir)?;
            create_dir_all(&dep_include_dir)?;
            let linkage = dep.linkage.unwrap_or(linkage);
            if dep.path.is_relative() {
                let dep_path = project_dir.as_ref().join(&dep.path);
                project.build_libraries(&dep_lib_dir, &dep_path, profile_name, linkage)?;
                project.copy_headers(dep_include_dir, &dep_path)?;
            } else {
                project.build_libraries(&dep_lib_dir, &dep.path, profile_name, linkage)?;
                project.copy_headers(dep_include_dir, &dep.path)?;
            }

            let mut lib_dir = OsString::from("-L");
            lib_dir.push(&dep_lib_dir);
            lib_dirs.push(lib_dir);

            for lib in &project.lib {
                let mut lib_arg = OsString::from("-l");
                lib_arg.push(&lib.target.name);
                libs.push(lib_arg);
            }
        }

        Ok((include_dir, lib_dirs, libs))
    }

    fn with_build_env<F: FnOnce(&BuildEnv) -> GocarResult<()>>(&self, target_dir: &Path, project_dir: &Path, profile_name: &str, linkage: LibraryType, f: F) -> GocarResult<()> {
        let profile = self.profiles.get(profile_name).ok_or(Error::InvalidProfileName)?;
        let (include_dir, lib_dirs, libs) = self.build_dependencies(target_dir, project_dir, profile_name, linkage)?;
        let strip_prefix = std::env::current_dir().unwrap_or_else(|_| PathBuf::new());
        let headers_only = self.headers_only.iter().map(|path| canonicalize_custom_wd(path, project_dir)).collect::<Result<_, _>>()?;
        let include_dirs = self.include_dirs
            .iter()
            .map(|path| canonicalize_custom_wd(path, project_dir))
            .map(|dir| dir.map(|dir| {
                let mut opt = OsString::from("-I");
                opt.push(dir);
                opt
            }))
            .collect::<Result<Vec<_>, _>>()?;

        let env = BuildEnv {
            target_dir: target_dir,
            project_dir: project_dir,
            include_dir: &include_dir,
            include_dirs: &include_dirs,
            lib_dirs: &lib_dirs,
            libs: &libs,
            profile,
            project: self,
            strip_prefix: &strip_prefix,
            headers_only: &headers_only,
            os: OsSpec::linux(),
        };

        f(&env)
    }

    fn build_libs(&self, env: &BuildEnv, linkage: LibraryType) -> GocarResult<()> {
        for lib in &self.lib {
            lib.build(&env, linkage)?;
        }

        Ok(())
    }

    fn build_bins(&self, env: &BuildEnv) -> GocarResult<()> {
        for bin in &self.bin {
            bin.build(&env)?;
        }

        Ok(())
    }

    pub fn build<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile_name: &str, linkage: LibraryType) -> GocarResult<()> {
        self.with_build_env(target_dir.as_ref(), project_dir.as_ref(), profile_name, linkage, |env| {
            self.build_libs(env, linkage)?;
            self.build_bins(env)
        })
    }

    pub fn build_libraries<TP: AsRef<Path>, PP: AsRef<Path>>(&self, target_dir: TP, project_dir: PP, profile_name: &str, linkage: LibraryType) -> GocarResult<()> {
        self.with_build_env(target_dir.as_ref(), project_dir.as_ref(), profile_name, linkage, |env| {
            self.build_libs(env, linkage)
        })
    }

    pub fn copy_headers<TP: AsRef<Path>, PP:AsRef<Path>>(&self, target_dir: TP, project_dir: PP) -> GocarResult<()> {
        for lib in &self.lib {
            for header_relative in &lib.public_headers {
                let header = [project_dir.as_ref(), "src".as_ref(), header_relative.as_ref()].iter().collect::<PathBuf>();
                let dest = [target_dir.as_ref(), header_relative.file_name().unwrap().as_ref()].iter().collect::<PathBuf>();
                std::fs::copy(&header, dest).err_ctx(|| (header, "copy file"))?;
            }
        }

        Ok(())
    }
}
