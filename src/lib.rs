use std::collections::HashMap;
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};

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
pub fn unit_to_obj<P: AsRef<Path> + Into<PathBuf>>(path: P) -> Option<PathBuf> {
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
